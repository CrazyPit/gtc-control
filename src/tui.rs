//! Interactive terminal UI.
//!
//! Full-screen ventilation panel powered by ratatui. The backend is a
//! single "Modbus actor" task that owns the [`crate::modbus::TcpClient`]
//! and serialises all device traffic through one TCP session — the
//! GTC EM-LAN module only tolerates a single Modbus TCP slot, so we
//! never open a parallel connection. The actor accepts two kinds of
//! work:
//!
//! - periodic ticks that fire [`crate::app::poll_once`] and stream the
//!   snapshot through a `watch` channel;
//! - explicit write commands from the UI, each of which runs
//!   [`crate::app::set_value`] and immediately re-polls so the screen
//!   reflects the new value without waiting for the next tick.
//!
//! Rendering uses [`crate::status`] to decode the well-known registers
//! (firmware version, state word, errors) into a friendly view, and
//! exposes four controls — power, temperature setpoint, supply-fan
//! setpoint, exhaust-fan setpoint — driven by an arrow-key + Enter
//! state machine.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::time;
use tracing::warn;

use crate::app::{AppError, poll_once, set_value};
use crate::config::{
    self, Config, ConfigError, ModbusConfig, ModeVisibility, PollConfig, UiConfig,
};
use crate::domain::{
    ModeSelection, RegisterDef, RegisterKind, RegisterValueType, Snapshot, SnapshotEntry,
};
use crate::status::{self, ActiveMode, DeviceState, OperationPhase, Priority, StatusView};

// =========================================================================
// Constants
// =========================================================================

const STATUS_DECODED_NAMES: &[&str] = &[
    "firmware_version",
    "state_word_0",
    "state_word_1",
    "error_code",
    "error_code_aux",
    "temp_setpoint",
    "mode_system",
];

const INLINE_TARGET_PAIRS: &[(&str, &str)] = &[
    ("fan_speed_supply_current", "fan_speed_supply"),
    ("fan_speed_exhaust_current", "fan_speed_exhaust"),
];

const LABEL_WIDTH: usize = 18;
const STATUS_LABEL_WIDTH: usize = 12;
const TOAST_LIFESPAN: Duration = Duration::from_secs(3);

// =========================================================================
// Public API
// =========================================================================

/// Errors surfaced by [`run`].
#[derive(Debug, Error)]
pub enum TuiError {
    /// Failed to read/write the terminal or set raw mode.
    #[error("terminal I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Launch the interactive terminal UI.
///
/// Returns once the user quits (`q` / `Esc` / `Ctrl-C`) or the
/// terminal is closed. The background actor task is signalled to stop
/// and joined before this function returns.
///
/// # Errors
/// Returns [`TuiError::Io`] on any failure to enter raw mode, draw, or
/// restore the terminal.
pub async fn run(cfg: Config) -> Result<(), TuiError> {
    let (snap_tx, snap_rx) = watch::channel(PollUpdate::Pending);
    let (write_tx, write_rx) = mpsc::channel::<WriteResult>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<ActorCommand>(8);

    let registers = cfg.registers.clone();
    let modbus_cfg = cfg.modbus.clone();
    let interval = Duration::from_secs(cfg.poll.interval_seconds.max(1));
    let actor = tokio::spawn(modbus_actor(
        modbus_cfg, registers, interval, snap_tx, write_tx, cmd_rx,
    ));

    let ui_result = render_loop(cfg, snap_rx, write_rx, cmd_tx.clone(), interval).await;

    let _ = cmd_tx.send(ActorCommand::Shutdown).await;
    if let Err(err) = actor.await {
        warn!(%err, "TUI actor task did not shut down cleanly");
    }
    ui_result
}

// =========================================================================
// Actor — Modbus owner
// =========================================================================

enum ActorCommand {
    Write {
        control: ControlId,
        register: String,
        value: String,
    },
    Shutdown,
}

#[derive(Clone)]
enum PollUpdate {
    Pending,
    Ok {
        snapshot: Snapshot,
        polled_at: Instant,
    },
    Err {
        message: String,
        failed_at: Instant,
    },
}

#[derive(Debug)]
struct WriteResult {
    control: ControlId,
    value: String,
    outcome: Result<(), String>,
    completed_at: Instant,
}

async fn modbus_actor(
    modbus_cfg: ModbusConfig,
    registers: Vec<RegisterDef>,
    interval: Duration,
    snap_tx: watch::Sender<PollUpdate>,
    write_tx: mpsc::Sender<WriteResult>,
    mut cmd_rx: mpsc::Receiver<ActorCommand>,
) {
    let mut client = crate::modbus::TcpClient::new(modbus_cfg);
    let mut ticker = time::interval(interval);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    None | Some(ActorCommand::Shutdown) => return,
                    Some(ActorCommand::Write { control, register, value }) => {
                        let result = set_value(&mut client, &registers, &register, &value)
                            .await
                            .map_err(|e| e.to_string());
                        let ok = result.is_ok();
                        let _ = write_tx
                            .send(WriteResult {
                                control,
                                value: value.clone(),
                                outcome: result,
                                completed_at: Instant::now(),
                            })
                            .await;
                        if ok {
                            let update = run_poll(&mut client, &registers).await;
                            let _ = snap_tx.send(update);
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                let update = run_poll(&mut client, &registers).await;
                if snap_tx.send(update).is_err() { return; }
            }
        }
    }
}

async fn run_poll(client: &mut crate::modbus::TcpClient, registers: &[RegisterDef]) -> PollUpdate {
    match poll_once(client, registers).await {
        Ok(snapshot) => PollUpdate::Ok {
            snapshot,
            polled_at: Instant::now(),
        },
        Err(err) => PollUpdate::Err {
            message: short_error(&err),
            failed_at: Instant::now(),
        },
    }
}

fn short_error(err: &AppError) -> String {
    err.to_string()
}

// =========================================================================
// Controls model
// =========================================================================

/// Identity of an editable / toggleable control. Each variant maps to
/// a canonical register name and a specific edit behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlId {
    Power,
    Mode,
    Setpoint,
    SupplyFan,
    ExhaustFan,
}

impl ControlId {
    fn register(self) -> &'static str {
        match self {
            Self::Power => "power",
            Self::Mode => "mode_system",
            Self::Setpoint => "temp_setpoint",
            Self::SupplyFan => "fan_speed_supply",
            Self::ExhaustFan => "fan_speed_exhaust",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Power => "Power",
            Self::Mode => "Mode",
            Self::Setpoint => "Setpoint",
            Self::SupplyFan => "Supply fan",
            Self::ExhaustFan => "Exhaust fan",
        }
    }

    fn is_toggle(self) -> bool {
        matches!(self, Self::Power | Self::Mode)
    }

    fn step(self) -> f32 {
        match self {
            Self::Setpoint => 0.5,
            Self::SupplyFan | Self::ExhaustFan => 1.0,
            Self::Power | Self::Mode => 0.0,
        }
    }

    fn range(self) -> (f32, f32) {
        match self {
            Self::Setpoint => (15.0, 30.0),
            // Fan setpoints must never be `0`: an unfanned heater
            // burns out within seconds. Enforced symmetrically in
            // `app::set_value` for the CLI path.
            Self::SupplyFan | Self::ExhaustFan => (1.0, 10.0),
            Self::Power | Self::Mode => (0.0, 1.0),
        }
    }

    fn decimals(self) -> usize {
        match self {
            Self::Setpoint => 1,
            Self::SupplyFan | Self::ExhaustFan | Self::Power | Self::Mode => 0,
        }
    }
}

fn active_controls(registers: &[RegisterDef], ui: &UiConfig) -> Vec<ControlId> {
    let names: HashSet<&str> = registers
        .iter()
        .filter(|r| r.writable)
        .map(|r| r.name.as_str())
        .collect();
    let mut out = Vec::new();
    for candidate in [
        ControlId::Power,
        ControlId::Mode,
        ControlId::Setpoint,
        ControlId::SupplyFan,
        ControlId::ExhaustFan,
    ] {
        if !names.contains(candidate.register()) {
            continue;
        }
        if matches!(candidate, ControlId::ExhaustFan) && !ui.exhaust_fan {
            continue;
        }
        out.push(candidate);
    }
    out
}

/// Display label for the mode picker. Matches the four-option mode
/// dropdown in the GTC mobile app: Ventilation / Heating / Cooling /
/// Climate.
fn mode_ui_label(mode: ModeSelection) -> &'static str {
    match mode {
        ModeSelection::Ventilation => "Ventilation",
        ModeSelection::Heating => "Heating",
        ModeSelection::Cooling => "Cooling",
        ModeSelection::Auto => "Climate",
    }
}

fn mode_color(mode: ModeSelection) -> Color {
    match mode {
        ModeSelection::Ventilation => Color::Blue,
        ModeSelection::Heating => Color::Yellow,
        ModeSelection::Cooling => Color::Cyan,
        ModeSelection::Auto => Color::Green,
    }
}

/// The mode picker order: Ventilation is always present, the other
/// three follow per [`UiConfig::modes`].
fn mode_picker_options(vis: ModeVisibility) -> Vec<ModeSelection> {
    let mut out = vec![ModeSelection::Ventilation];
    if vis.heating {
        out.push(ModeSelection::Heating);
    }
    if vis.cooling {
        out.push(ModeSelection::Cooling);
    }
    if vis.climate {
        out.push(ModeSelection::Auto);
    }
    out
}

/// Advance to the next mode the user has enabled. If `current` was
/// hidden by the user (i.e. it isn't in the picker list), the cycle
/// resumes from the first enabled entry.
fn next_mode_in_cycle(current: ModeSelection, vis: ModeVisibility) -> ModeSelection {
    let options = mode_picker_options(vis);
    let idx = options.iter().position(|m| *m == current).unwrap_or(0);
    options[(idx + 1) % options.len()]
}

fn format_for(control: ControlId, value: f32) -> String {
    format!("{value:.decimals$}", decimals = control.decimals())
}

fn validate_buffer(control: ControlId, text: &str) -> Result<(), String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("value is empty".into());
    }
    let parsed: f32 = trimmed
        .parse()
        .map_err(|_| format!("`{trimmed}` is not a number"))?;
    let (min, max) = control.range();
    if !(min..=max).contains(&parsed) {
        return Err(format!("must be in {min}..={max}"));
    }
    Ok(())
}

// =========================================================================
// UI state & mode
// =========================================================================

struct UiState {
    cfg: Config,
    update: PollUpdate,
    controls: Vec<ControlId>,
    selection: usize,
    mode: UiMode,
    toast: Option<Toast>,
    pending_write: Option<PendingWrite>,
    interval: Duration,
}

enum UiMode {
    Normal,
    Editing(EditBuffer),
    Settings(SettingsState),
}

struct EditBuffer {
    control: ControlId,
    text: String,
}

/// State backing the full-screen Settings view. A draft snapshot of
/// the user-tunable config that the screen mutates in-place; on
/// close [`close_settings`] writes the draft back to disk via
/// [`config::save_user_config`] and replaces the live state values.
struct SettingsState {
    draft_modbus: ModbusConfig,
    draft_poll: PollConfig,
    draft_ui: UiConfig,
    items: Vec<SettingsItem>,
    cursor: usize,
    editing: Option<SettingsEditBuffer>,
}

/// One navigable row in the Settings screen. Section headings are
/// not items — they are emitted around items at render time.
enum SettingsItem {
    Host,
    Port,
    PollInterval,
    /// Temperature visibility toggle for the register with this name.
    Temperature(String),
    ModeHeating,
    ModeCooling,
    ModeClimate,
    ExhaustFan,
}

struct SettingsEditBuffer {
    item_index: usize,
    text: String,
}

struct PendingWrite {
    control: ControlId,
    value: String,
}

struct Toast {
    text: String,
    kind: ToastKind,
    shown_at: Instant,
}

#[derive(Clone, Copy)]
enum ToastKind {
    Success,
    Error,
}

impl UiState {
    fn new(cfg: Config, interval: Duration) -> Self {
        let controls = active_controls(&cfg.registers, &cfg.ui);
        Self {
            cfg,
            update: PollUpdate::Pending,
            controls,
            selection: 0,
            mode: UiMode::Normal,
            toast: None,
            pending_write: None,
            interval,
        }
    }

    fn refresh_controls(&mut self) {
        self.controls = active_controls(&self.cfg.registers, &self.cfg.ui);
        if self.selection >= self.controls.len() {
            self.selection = self.controls.len().saturating_sub(1);
        }
    }

    fn current_snapshot(&self) -> Option<&Snapshot> {
        match &self.update {
            PollUpdate::Ok { snapshot, .. } => Some(snapshot),
            _ => None,
        }
    }

    fn current_view(&self) -> Option<StatusView> {
        self.current_snapshot().map(status::build_status)
    }

    fn selected_control(&self) -> Option<ControlId> {
        self.controls.get(self.selection).copied()
    }

    fn is_selected(&self, control: ControlId) -> bool {
        self.selected_control() == Some(control)
    }

    fn editing(&self, control: ControlId) -> Option<&EditBuffer> {
        match &self.mode {
            UiMode::Editing(buf) if buf.control == control => Some(buf),
            _ => None,
        }
    }

    fn pending_for(&self, control: ControlId) -> Option<&PendingWrite> {
        self.pending_write.as_ref().filter(|p| p.control == control)
    }

    fn prune_toast(&mut self) {
        if let Some(toast) = &self.toast
            && toast.shown_at.elapsed() > TOAST_LIFESPAN
        {
            self.toast = None;
        }
    }
}

// =========================================================================
// Render loop
// =========================================================================

async fn render_loop(
    cfg: Config,
    mut snap_rx: watch::Receiver<PollUpdate>,
    mut write_rx: mpsc::Receiver<WriteResult>,
    cmd_tx: mpsc::Sender<ActorCommand>,
    interval: Duration,
) -> Result<(), TuiError> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut events = EventStream::new();

    let mut state = UiState::new(cfg, interval);
    state.update = snap_rx.borrow().clone();

    loop {
        state.prune_toast();
        terminal.draw(|f| draw_ui(f, &state))?;

        let redraw_tick = time::sleep(Duration::from_millis(500));
        tokio::pin!(redraw_tick);

        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        if handle_key(&mut state, &key, &cmd_tx).await {
                            return Ok(());
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => warn!(%err, "terminal event stream error"),
                    None => return Ok(()),
                }
            }
            res = snap_rx.changed() => {
                if res.is_err() { return Ok(()); }
                state.update = snap_rx.borrow().clone();
            }
            Some(result) = write_rx.recv() => {
                handle_write_result(&mut state, result);
            }
            () = &mut redraw_tick => {}
        }
    }
}

async fn handle_key(
    state: &mut UiState,
    key: &KeyEvent,
    cmd_tx: &mpsc::Sender<ActorCommand>,
) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if is_global_quit(key) && matches!(state.mode, UiMode::Normal) {
        return true;
    }

    match &mut state.mode {
        UiMode::Normal => handle_normal_key(state, key, cmd_tx).await,
        UiMode::Editing(_) => handle_edit_key(state, key, cmd_tx).await,
        UiMode::Settings(_) => {
            handle_settings_key(state, key);
            false
        }
    }
}

fn is_global_quit(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => true,
        KeyCode::Char('c' | 'C') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

async fn handle_normal_key(
    state: &mut UiState,
    key: &KeyEvent,
    cmd_tx: &mpsc::Sender<ActorCommand>,
) -> bool {
    match key.code {
        KeyCode::Up => {
            state.selection = state.selection.saturating_sub(1);
        }
        KeyCode::Down => {
            if state.selection + 1 < state.controls.len() {
                state.selection += 1;
            }
        }
        KeyCode::Char('s' | 'S') => {
            open_settings(state);
        }
        KeyCode::Enter => {
            let Some(control) = state.selected_control() else {
                return false;
            };
            if control.is_toggle() {
                toggle_control(state, control, cmd_tx).await;
            } else {
                let current = current_value_for(control, state.current_snapshot());
                state.mode = UiMode::Editing(EditBuffer {
                    control,
                    text: current,
                });
            }
        }
        _ => {}
    }
    false
}

async fn handle_edit_key(
    state: &mut UiState,
    key: &KeyEvent,
    cmd_tx: &mpsc::Sender<ActorCommand>,
) -> bool {
    let UiMode::Editing(buf) = &mut state.mode else {
        return false;
    };
    let control = buf.control;
    match key.code {
        KeyCode::Esc => {
            state.mode = UiMode::Normal;
        }
        KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.mode = UiMode::Normal;
            return true;
        }
        KeyCode::Enter => match validate_buffer(control, &buf.text) {
            Ok(()) => {
                let value = buf.text.trim().to_owned();
                state.mode = UiMode::Normal;
                dispatch_write(state, control, value, cmd_tx).await;
            }
            Err(msg) => {
                state.toast = Some(Toast {
                    text: msg,
                    kind: ToastKind::Error,
                    shown_at: Instant::now(),
                });
            }
        },
        KeyCode::Up => apply_step(buf, 1.0),
        KeyCode::Down => apply_step(buf, -1.0),
        KeyCode::Backspace => {
            buf.text.pop();
        }
        KeyCode::Char(c) if c.is_ascii_digit() || c == '.' || c == '-' => {
            buf.text.push(c);
        }
        _ => {}
    }
    false
}

fn apply_step(buf: &mut EditBuffer, sign: f32) {
    let parsed: f32 = buf.text.trim().parse().unwrap_or(0.0);
    let stepped = parsed + sign * buf.control.step();
    let (min, max) = buf.control.range();
    let clamped = stepped.clamp(min, max);
    buf.text = format_for(buf.control, clamped);
}

async fn toggle_control(
    state: &mut UiState,
    control: ControlId,
    cmd_tx: &mpsc::Sender<ActorCommand>,
) {
    let new_value = match control {
        ControlId::Power => {
            let currently_on = state
                .current_view()
                .and_then(|v| v.state.map(|s| s.power_on))
                .unwrap_or(false);
            if currently_on { "0" } else { "1" }.to_owned()
        }
        ControlId::Mode => {
            let current = state
                .current_view()
                .and_then(|v| v.mode_selection)
                .unwrap_or(ModeSelection::Ventilation);
            next_mode_in_cycle(current, state.cfg.ui.modes)
                .label()
                .to_owned()
        }
        // Non-toggle controls enter edit mode and never reach this
        // function; keep the match exhaustive without contriving a
        // value to write.
        ControlId::Setpoint | ControlId::SupplyFan | ControlId::ExhaustFan => return,
    };
    dispatch_write(state, control, new_value, cmd_tx).await;
}

async fn dispatch_write(
    state: &mut UiState,
    control: ControlId,
    value: String,
    cmd_tx: &mpsc::Sender<ActorCommand>,
) {
    state.pending_write = Some(PendingWrite {
        control,
        value: value.clone(),
    });
    if cmd_tx
        .send(ActorCommand::Write {
            control,
            register: control.register().to_owned(),
            value,
        })
        .await
        .is_err()
    {
        state.pending_write = None;
        state.toast = Some(Toast {
            text: "actor channel closed".into(),
            kind: ToastKind::Error,
            shown_at: Instant::now(),
        });
    }
}

fn handle_write_result(state: &mut UiState, result: WriteResult) {
    if let Some(pending) = &state.pending_write
        && pending.control == result.control
        && pending.value == result.value
    {
        state.pending_write = None;
    }
    match result.outcome {
        Ok(()) => {
            state.toast = Some(Toast {
                text: format!("{}: {}", result.control.label(), result.value),
                kind: ToastKind::Success,
                shown_at: result.completed_at,
            });
        }
        Err(msg) => {
            state.toast = Some(Toast {
                text: format!("{}: {msg}", result.control.label()),
                kind: ToastKind::Error,
                shown_at: result.completed_at,
            });
        }
    }
}

fn current_value_for(control: ControlId, snapshot: Option<&Snapshot>) -> String {
    let Some(snapshot) = snapshot else {
        return String::new();
    };
    match control {
        ControlId::Setpoint => snapshot
            .entries
            .iter()
            .find(|e| e.name == "temp_setpoint")
            .and_then(|e| match e.value {
                crate::domain::RegisterValue::Temperature(t) => Some(format!("{t:.1}")),
                _ => None,
            })
            .unwrap_or_default(),
        ControlId::SupplyFan => snapshot
            .entries
            .iter()
            .find(|e| e.name == "fan_speed_supply")
            .and_then(|e| match e.value {
                crate::domain::RegisterValue::U16(v) => Some(v.to_string()),
                _ => None,
            })
            .unwrap_or_default(),
        ControlId::ExhaustFan => snapshot
            .entries
            .iter()
            .find(|e| e.name == "fan_speed_exhaust")
            .and_then(|e| match e.value {
                crate::domain::RegisterValue::U16(v) => Some(v.to_string()),
                _ => None,
            })
            .unwrap_or_default(),
        ControlId::Power | ControlId::Mode => String::new(),
    }
}

// =========================================================================
// Rendering
// =========================================================================

fn draw_ui(f: &mut ratatui::Frame, state: &UiState) {
    if matches!(state.mode, UiMode::Settings(_)) {
        draw_settings_ui(f, state);
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(f.area());

    let view = state.current_view();
    draw_header(f, chunks[0], state, view.as_ref());
    draw_body(f, chunks[1], state, view.as_ref());
    draw_footer(f, chunks[2], state);
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, state: &UiState, view: Option<&StatusView>) {
    let endpoint = format!(
        "{}:{}  unit {}",
        state.cfg.modbus.host, state.cfg.modbus.port, state.cfg.modbus.unit_id
    );
    let mut info_spans: Vec<Span> = Vec::new();
    if let Some(fw) = view.and_then(|v| v.firmware) {
        info_spans.push(Span::styled(
            "Firmware ",
            Style::default().add_modifier(Modifier::DIM),
        ));
        info_spans.push(Span::styled(
            fw.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        info_spans.push(Span::raw("    "));
    }
    info_spans.extend(poll_status_spans(&state.update, state.interval));
    let header = Paragraph::new(vec![Line::from(endpoint), Line::from(info_spans)]).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .title(" GTC Control ")
            .title_alignment(Alignment::Left),
    );
    f.render_widget(header, area);
}

fn poll_status_spans(update: &PollUpdate, interval: Duration) -> Vec<Span<'_>> {
    match update {
        PollUpdate::Pending => vec![
            Span::styled("● ", Style::default().fg(Color::Yellow)),
            Span::raw("connecting..."),
        ],
        PollUpdate::Ok { polled_at, .. } => {
            let elapsed = polled_at.elapsed();
            let next_in = interval.saturating_sub(elapsed);
            vec![
                Span::styled("● ", Style::default().fg(Color::Green)),
                Span::raw(format!(
                    "polled {}s ago · next refresh in {}s",
                    elapsed.as_secs(),
                    next_in.as_secs(),
                )),
            ]
        }
        PollUpdate::Err { message, failed_at } => vec![
            Span::styled("● ", Style::default().fg(Color::Red)),
            Span::raw(format!(
                "error {}s ago: {message}",
                failed_at.elapsed().as_secs()
            )),
        ],
    }
}

fn draw_body(f: &mut ratatui::Frame, area: Rect, state: &UiState, view: Option<&StatusView>) {
    let lines = match &state.update {
        PollUpdate::Pending => vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Waiting for the first poll to complete...",
                Style::default().add_modifier(Modifier::DIM),
            )),
        ],
        PollUpdate::Err { message, .. } if view.is_none() => vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Last poll failed",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::raw(format!("    {message}"))),
        ],
        _ => {
            let mut lines = Vec::with_capacity(64);
            if let Some(view) = view {
                build_status_lines(&mut lines, view, state);
            }
            if let Some(snapshot) = state.current_snapshot() {
                build_data_lines(&mut lines, snapshot, state);
            }
            lines
        }
    };

    let body = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
    f.render_widget(body, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, state: &UiState) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);

    let toast_line = match &state.toast {
        Some(t) => {
            let (glyph, color) = match t.kind {
                ToastKind::Success => ("✓", Color::Green),
                ToastKind::Error => ("✗", Color::Red),
            };
            Line::from(vec![
                Span::raw("  "),
                Span::styled(glyph, Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    t.text.clone(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ])
        }
        None => match &state.pending_write {
            Some(p) => Line::from(vec![
                Span::raw("  "),
                Span::styled("⌛ ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("writing {}: {}...", p.control.label(), p.value),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            None => Line::from(""),
        },
    };

    let hint_line = match &state.mode {
        UiMode::Normal => Line::from(vec![
            Span::raw("  "),
            keybind("↑↓"),
            Span::raw(" navigate · "),
            keybind("Enter"),
            Span::raw(" "),
            Span::raw(match state.selected_control() {
                Some(c) if c.is_toggle() => "toggle",
                Some(_) => "edit",
                None => "—",
            }),
            Span::raw(" · "),
            keybind("s"),
            Span::raw(" settings · "),
            keybind("q"),
            Span::raw(" quit"),
        ]),
        UiMode::Editing(buf) => {
            let step = format_for(buf.control, buf.control.step());
            Line::from(vec![
                Span::raw("  "),
                keybind("↑↓"),
                Span::raw(format!(" ±{step} · ")),
                Span::raw("type digits/. · "),
                keybind("Enter"),
                Span::raw(" save · "),
                keybind("Esc"),
                Span::raw(" cancel"),
            ])
        }
        // Footer is not drawn while the Settings screen owns the
        // whole terminal — `draw_ui` short-circuits before reaching
        // this code path.
        UiMode::Settings(_) => Line::from(""),
    };

    f.render_widget(
        Paragraph::new(toast_line).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(hint_line).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[1],
    );
}

fn keybind(key: &str) -> Span<'static> {
    Span::styled(
        format!("[{key}]"),
        Style::default().add_modifier(Modifier::BOLD),
    )
}

// =========================================================================
// Status section (with control rows)
// =========================================================================

fn build_status_lines(lines: &mut Vec<Line<'static>>, view: &StatusView, state: &UiState) {
    push_section_header(lines, "Status");

    if let Some(device_state) = view.state {
        lines.push(power_line(device_state, state));
        if state.controls.contains(&ControlId::Mode) {
            lines.push(mode_selection_line(view, device_state, state));
        } else {
            lines.push(status_line("Mode", mode_value_spans(device_state)));
        }
        if !matches!(device_state.priority, Priority::None) {
            lines.push(status_line(
                "Priority",
                priority_value_spans(device_state.priority),
            ));
        }
        if device_state.timer_today || device_state.timer_week {
            lines.push(status_line("Schedule", schedule_value_spans(device_state)));
        }
    }

    lines.push(setpoint_line(view.temperature_setpoint, state));

    if let Some(phase) = view.phase {
        lines.push(status_line("Phase", phase_value_spans(phase)));
    }

    lines.push(status_line("Errors", errors_value_spans(&view.errors)));
    for label in view.errors.iter().skip(1) {
        lines.push(extra_message_line(label, Color::Red, "  • "));
    }

    if !view.notes.is_empty() {
        lines.push(status_line("Notes", notes_first_line_spans(&view.notes)));
        for label in view.notes.iter().skip(1) {
            lines.push(extra_message_line(label, Color::Yellow, "  • "));
        }
    }

    lines.push(Line::from(""));
}

fn power_line(device_state: DeviceState, state: &UiState) -> Line<'static> {
    let selected = state.is_selected(ControlId::Power);
    let pending = state.pending_for(ControlId::Power);
    let display_on = match pending {
        Some(p) => p.value == "1",
        None => device_state.power_on,
    };
    let mut spans = control_row_prefix("Power", selected);
    let glyph_color = if display_on { Color::Green } else { Color::Red };
    let text_color = glyph_color;
    let label = if display_on { "ON" } else { "OFF" };
    let suffix = if device_state.transitioning {
        if display_on {
            " (starting)"
        } else {
            " (stopping)"
        }
    } else {
        ""
    };
    spans.push(Span::styled("● ", Style::default().fg(glyph_color)));
    spans.push(Span::styled(
        format!("{label}{suffix}"),
        Style::default().fg(text_color).add_modifier(Modifier::BOLD),
    ));
    if selected && pending.is_none() {
        spans.push(hint_span(" Enter — toggle"));
    } else if pending.is_some() {
        spans.push(hint_span(" writing..."));
    }
    Line::from(spans)
}

fn mode_selection_line(
    view: &StatusView,
    device_state: DeviceState,
    state: &UiState,
) -> Line<'static> {
    let selected = state.is_selected(ControlId::Mode);
    let pending = state.pending_for(ControlId::Mode);
    let pending_mode = pending.and_then(|p| ModeSelection::parse(&p.value).ok());
    let display_mode = pending_mode.or(view.mode_selection);
    let mut spans = control_row_prefix("Mode", selected);
    match display_mode {
        Some(mode) => {
            spans.push(Span::styled(
                mode_ui_label(mode).to_owned(),
                Style::default()
                    .fg(mode_color(mode))
                    .add_modifier(Modifier::BOLD),
            ));
            // Only "Auto" lets the controller pick heat vs cool; for
            // the heat-only / cool-only modes the label is already
            // self-describing.
            if matches!(mode, ModeSelection::Auto) {
                let active = device_state.active_mode();
                let label = match active {
                    ActiveMode::Heating => Some("heating now"),
                    ActiveMode::Cooling => Some("cooling now"),
                    ActiveMode::Ventilation => None,
                };
                if let Some(label) = label {
                    spans.push(Span::styled(
                        format!("  ({label})"),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
            }
        }
        None => spans.push(Span::styled(
            "—",
            Style::default().add_modifier(Modifier::DIM),
        )),
    }
    if selected && pending.is_none() {
        spans.push(hint_span(" Enter — cycle"));
    } else if pending.is_some() {
        spans.push(hint_span(" writing..."));
    }
    Line::from(spans)
}

fn setpoint_line(target: Option<f32>, state: &UiState) -> Line<'static> {
    let selected = state.is_selected(ControlId::Setpoint);
    let editing = state.editing(ControlId::Setpoint);
    let pending = state.pending_for(ControlId::Setpoint);
    let mut spans = control_row_prefix("Setpoint", selected);

    if let Some(buf) = editing {
        spans.push(Span::styled(
            format!("[{}█] °C", buf.text),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    let display = pending
        .map(|p| p.value.clone())
        .or_else(|| target.map(|t| format!("{t:.1}")));
    match display {
        Some(text) => spans.push(Span::styled(
            format!("{text} °C"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )),
        None => spans.push(Span::styled(
            "—",
            Style::default().add_modifier(Modifier::DIM),
        )),
    }
    if selected && pending.is_none() {
        spans.push(hint_span(" Enter — edit"));
    } else if pending.is_some() {
        spans.push(hint_span(" writing..."));
    }
    Line::from(spans)
}

fn control_row_prefix(label: &str, selected: bool) -> Vec<Span<'static>> {
    let cursor = if selected { "  ▶ " } else { "    " };
    let label_style = if selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    vec![
        Span::styled(
            cursor.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{label:<STATUS_LABEL_WIDTH$}  "), label_style),
    ]
}

fn hint_span(text: &str) -> Span<'static> {
    Span::styled(
        text.to_owned(),
        Style::default().add_modifier(Modifier::DIM),
    )
}

fn status_line(label: &str, value_spans: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = Vec::with_capacity(value_spans.len() + 2);
    spans.push(Span::raw("    "));
    spans.push(Span::styled(
        format!("{label:<STATUS_LABEL_WIDTH$}  "),
        Style::default().add_modifier(Modifier::DIM),
    ));
    spans.extend(value_spans);
    Line::from(spans)
}

fn extra_message_line(message: &str, color: Color, prefix: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
        Span::raw(format!("{:width$}", "", width = STATUS_LABEL_WIDTH + 2)),
        Span::styled(prefix.to_owned(), Style::default().fg(color)),
        Span::styled(message.to_owned(), Style::default().fg(color)),
    ])
}

fn push_section_header(lines: &mut Vec<Line<'static>>, label: &str) {
    lines.push(Line::from(Span::styled(
        format!("  {label}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
}

fn mode_value_spans(state: DeviceState) -> Vec<Span<'static>> {
    let mode = state.active_mode();
    let color = match mode {
        ActiveMode::Heating => Color::Yellow,
        ActiveMode::Cooling => Color::Cyan,
        ActiveMode::Ventilation => Color::Gray,
    };
    let mut spans = vec![Span::styled(
        mode.label().to_owned(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    let installed = match (state.heating_available, state.cooling_available) {
        (true, true) => Some("heat + cool installed"),
        (false, false) => Some("no thermal stage installed"),
        (true, false) | (false, true) => None,
    };
    if let Some(label) = installed {
        spans.push(Span::styled(
            format!("  ({label})"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    spans
}

fn priority_value_spans(priority: Priority) -> Vec<Span<'static>> {
    vec![Span::styled(
        priority.label().to_owned(),
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )]
}

fn schedule_value_spans(state: DeviceState) -> Vec<Span<'static>> {
    let mut parts = Vec::new();
    if state.timer_today {
        parts.push("today");
    }
    if state.timer_week {
        parts.push("week");
    }
    vec![
        Span::styled("⏱ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            format!("timer active ({})", parts.join(", ")),
            Style::default().fg(Color::Cyan),
        ),
    ]
}

fn phase_value_spans(phase: OperationPhase) -> Vec<Span<'static>> {
    let style = if matches!(phase, OperationPhase::Idle) {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    };
    vec![Span::styled(phase.label().to_owned(), style)]
}

fn errors_value_spans(errors: &[&'static str]) -> Vec<Span<'static>> {
    if errors.is_empty() {
        return vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::styled("none active", Style::default().fg(Color::Green)),
        ];
    }
    vec![
        Span::styled("✗ ", Style::default().fg(Color::Red)),
        Span::styled(
            errors[0].to_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    ]
}

fn notes_first_line_spans(notes: &[&'static str]) -> Vec<Span<'static>> {
    vec![
        Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
        Span::styled(notes[0].to_owned(), Style::default().fg(Color::Yellow)),
    ]
}

// =========================================================================
// Data sections (Temperatures / Fans / Filters / ...)
// =========================================================================

fn build_data_lines(lines: &mut Vec<Line<'static>>, snapshot: &Snapshot, state: &UiState) {
    let decoded: HashSet<&str> = STATUS_DECODED_NAMES.iter().copied().collect();
    let inline_targets: HashSet<&str> = INLINE_TARGET_PAIRS.iter().map(|(_, t)| *t).collect();
    let registers = &state.cfg.registers;

    let label_lookup: BTreeMap<&str, &str> = registers
        .iter()
        .map(|r| {
            (
                r.name.as_str(),
                r.display_name.as_deref().unwrap_or(r.name.as_str()),
            )
        })
        .collect();
    let group_lookup: BTreeMap<&str, &str> = registers
        .iter()
        .map(|r| (r.name.as_str(), r.group.as_deref().unwrap_or("Other")))
        .collect();
    let entry_lookup: BTreeMap<&str, &SnapshotEntry> = snapshot
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    let mut sections: Vec<(&str, Vec<&SnapshotEntry>)> = Vec::new();
    for entry in &snapshot.entries {
        if decoded.contains(entry.name.as_str()) || inline_targets.contains(entry.name.as_str()) {
            continue;
        }
        if !data_entry_visible(&state.cfg, entry.name.as_str()) {
            continue;
        }
        let group = group_lookup
            .get(entry.name.as_str())
            .copied()
            .unwrap_or("Other");
        if group == "Controls" {
            continue;
        }
        if let Some(existing) = sections.iter_mut().find(|(g, _)| *g == group) {
            existing.1.push(entry);
        } else {
            sections.push((group, vec![entry]));
        }
    }

    for (group, entries) in sections {
        push_section_header(lines, group);
        for entry in entries {
            let label = label_lookup
                .get(entry.name.as_str())
                .copied()
                .unwrap_or(entry.name.as_str());
            let target_register = INLINE_TARGET_PAIRS
                .iter()
                .find(|(actual, _)| *actual == entry.name)
                .map(|(_, t)| *t);
            let target_entry = target_register.and_then(|t| entry_lookup.get(t).copied());
            let control = target_register.and_then(control_for_target);
            lines.push(data_line(label, entry, target_entry, control, state));
        }
        lines.push(Line::from(""));
    }
}

fn control_for_target(name: &str) -> Option<ControlId> {
    match name {
        "fan_speed_supply" => Some(ControlId::SupplyFan),
        "fan_speed_exhaust" => Some(ControlId::ExhaustFan),
        _ => None,
    }
}

/// Whether the row for `name` should appear in the body data
/// sections. Filters temperatures by [`UiConfig::temperatures`] and
/// the exhaust-fan rows (both current speed and setpoint) by
/// [`UiConfig::exhaust_fan`].
fn data_entry_visible(cfg: &Config, name: &str) -> bool {
    if matches!(name, "fan_speed_exhaust" | "fan_speed_exhaust_current") {
        return cfg.ui.exhaust_fan;
    }
    if let Some(reg) = cfg.register(name)
        && reg.kind == RegisterKind::Input
        && reg.value_type == RegisterValueType::TemperatureX10
    {
        return cfg.temperature_visible(name);
    }
    true
}

fn data_line(
    label: &str,
    entry: &SnapshotEntry,
    target: Option<&SnapshotEntry>,
    control: Option<ControlId>,
    state: &UiState,
) -> Line<'static> {
    let selected = control.is_some_and(|c| state.is_selected(c));
    let editing = control.and_then(|c| state.editing(c));
    let pending = control.and_then(|c| state.pending_for(c));

    let cursor = if selected { "  ▶ " } else { "    " };
    let mut spans = vec![
        Span::styled(
            cursor.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{label:<LABEL_WIDTH$}  "),
            if selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            },
        ),
    ];

    let mut value = entry.value.to_string();
    if let Some(unit) = entry.unit.as_deref() {
        value.push(' ');
        value.push_str(unit);
    }
    spans.push(Span::styled(
        value,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));

    if target.is_some() || control.is_some() {
        spans.push(Span::styled(
            "   →  target ".to_owned(),
            Style::default().add_modifier(Modifier::DIM),
        ));
        if let Some(buf) = editing {
            spans.push(Span::styled(
                format!("[{}█]", buf.text),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            let target_text = pending
                .map(|p| p.value.clone())
                .or_else(|| target.map(format_entry_value))
                .unwrap_or_else(|| "—".to_owned());
            spans.push(Span::styled(
                target_text,
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    if selected && editing.is_none() && pending.is_none() {
        spans.push(hint_span("  Enter — edit"));
    } else if pending.is_some() {
        spans.push(hint_span("  writing..."));
    }

    Line::from(spans)
}

fn format_entry_value(entry: &SnapshotEntry) -> String {
    let mut value = entry.value.to_string();
    if let Some(unit) = entry.unit.as_deref() {
        value.push(' ');
        value.push_str(unit);
    }
    value
}

// =========================================================================
// Settings screen
// =========================================================================

fn build_settings_items(cfg: &Config) -> Vec<SettingsItem> {
    let mut items = vec![
        SettingsItem::Host,
        SettingsItem::Port,
        SettingsItem::PollInterval,
    ];
    for reg in &cfg.registers {
        if reg.kind == RegisterKind::Input && reg.value_type == RegisterValueType::TemperatureX10 {
            items.push(SettingsItem::Temperature(reg.name.clone()));
        }
    }
    items.push(SettingsItem::ModeHeating);
    items.push(SettingsItem::ModeCooling);
    items.push(SettingsItem::ModeClimate);
    items.push(SettingsItem::ExhaustFan);
    items
}

fn open_settings(state: &mut UiState) {
    let items = build_settings_items(&state.cfg);
    state.mode = UiMode::Settings(SettingsState {
        draft_modbus: state.cfg.modbus.clone(),
        draft_poll: state.cfg.poll.clone(),
        draft_ui: state.cfg.ui.clone(),
        items,
        cursor: 0,
        editing: None,
    });
}

/// Commit the Settings draft: apply UI changes live, save to disk,
/// flag connection changes (apply on next launch).
fn close_settings(state: &mut UiState) {
    let UiMode::Settings(settings) = std::mem::replace(&mut state.mode, UiMode::Normal) else {
        return;
    };
    let connection_changed = settings.draft_modbus.host != state.cfg.modbus.host
        || settings.draft_modbus.port != state.cfg.modbus.port
        || settings.draft_modbus.unit_id != state.cfg.modbus.unit_id
        || settings.draft_modbus.timeout_ms != state.cfg.modbus.timeout_ms
        || settings.draft_poll.interval_seconds != state.cfg.poll.interval_seconds;

    state.cfg.modbus = settings.draft_modbus;
    state.cfg.poll = settings.draft_poll;
    state.cfg.ui = settings.draft_ui;
    state.refresh_controls();

    let toast = match config::save_user_config(&state.cfg) {
        Ok(()) => {
            let text = if connection_changed {
                "settings saved — restart to apply connection changes".to_owned()
            } else {
                "settings saved".to_owned()
            };
            Toast {
                text,
                kind: ToastKind::Success,
                shown_at: Instant::now(),
            }
        }
        Err(err) => Toast {
            text: settings_save_error(&err),
            kind: ToastKind::Error,
            shown_at: Instant::now(),
        },
    };
    state.toast = Some(toast);
}

fn settings_save_error(err: &ConfigError) -> String {
    format!("settings save failed: {err}")
}

fn handle_settings_key(state: &mut UiState, key: &KeyEvent) {
    let UiMode::Settings(settings) = &mut state.mode else {
        return;
    };
    if settings.editing.is_some() {
        handle_settings_edit_key(settings, key);
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('s' | 'S') => {
            close_settings(state);
        }
        KeyCode::Up => {
            settings.cursor = settings.cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            if settings.cursor + 1 < settings.items.len() {
                settings.cursor += 1;
            }
        }
        KeyCode::Char(' ') => toggle_settings_item(settings),
        KeyCode::Enter => start_settings_edit(settings),
        _ => {}
    }
}

fn handle_settings_edit_key(settings: &mut SettingsState, key: &KeyEvent) {
    let Some(buf) = settings.editing.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            settings.editing = None;
        }
        KeyCode::Enter => {
            commit_settings_edit(settings);
        }
        KeyCode::Backspace => {
            buf.text.pop();
        }
        KeyCode::Char(c) => {
            if settings_edit_accepts(&settings.items[buf.item_index], c) {
                buf.text.push(c);
            }
        }
        _ => {}
    }
}

fn settings_edit_accepts(item: &SettingsItem, c: char) -> bool {
    match item {
        SettingsItem::Host => !c.is_whitespace(),
        SettingsItem::Port | SettingsItem::PollInterval => c.is_ascii_digit(),
        _ => false,
    }
}

fn toggle_settings_item(settings: &mut SettingsState) {
    let Some(item) = settings.items.get(settings.cursor) else {
        return;
    };
    match item {
        SettingsItem::Host | SettingsItem::Port | SettingsItem::PollInterval => {
            // Numeric / string fields toggle via Enter, not Space.
        }
        SettingsItem::Temperature(name) => {
            let current = settings
                .draft_ui
                .temperatures
                .get(name)
                .copied()
                .unwrap_or(true);
            settings
                .draft_ui
                .temperatures
                .insert(name.clone(), !current);
        }
        SettingsItem::ModeHeating => {
            settings.draft_ui.modes.heating = !settings.draft_ui.modes.heating;
        }
        SettingsItem::ModeCooling => {
            settings.draft_ui.modes.cooling = !settings.draft_ui.modes.cooling;
        }
        SettingsItem::ModeClimate => {
            settings.draft_ui.modes.climate = !settings.draft_ui.modes.climate;
        }
        SettingsItem::ExhaustFan => {
            settings.draft_ui.exhaust_fan = !settings.draft_ui.exhaust_fan;
        }
    }
}

fn start_settings_edit(settings: &mut SettingsState) {
    let idx = settings.cursor;
    let Some(item) = settings.items.get(idx) else {
        return;
    };
    let text = match item {
        SettingsItem::Host => settings.draft_modbus.host.clone(),
        SettingsItem::Port => settings.draft_modbus.port.to_string(),
        SettingsItem::PollInterval => settings.draft_poll.interval_seconds.to_string(),
        // Bool items are toggled with Space, not edited.
        _ => return,
    };
    settings.editing = Some(SettingsEditBuffer {
        item_index: idx,
        text,
    });
}

fn commit_settings_edit(settings: &mut SettingsState) {
    let Some(buf) = settings.editing.as_ref() else {
        return;
    };
    let trimmed = buf.text.trim();
    let item = &settings.items[buf.item_index];
    let result: Result<(), String> = match item {
        SettingsItem::Host => {
            if trimmed.is_empty() {
                Err("host is empty".into())
            } else {
                settings.draft_modbus.host = trimmed.to_owned();
                Ok(())
            }
        }
        SettingsItem::Port => trimmed
            .parse::<u16>()
            .map(|p| settings.draft_modbus.port = p)
            .map_err(|e| e.to_string()),
        SettingsItem::PollInterval => match trimmed.parse::<u64>() {
            Ok(0) => Err("interval must be at least 1 second".to_owned()),
            Ok(v) => {
                settings.draft_poll.interval_seconds = v;
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        _ => Ok(()),
    };
    if result.is_ok() {
        settings.editing = None;
    }
}

fn draw_settings_ui(f: &mut ratatui::Frame, state: &UiState) {
    let UiMode::Settings(settings) = &state.mode else {
        return;
    };
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Settings ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::raw("   "),
        Span::styled(
            "edits saved on Esc",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]))
    .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(title, chunks[0]);

    let lines = build_settings_lines(settings, &state.cfg.registers);
    let body = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
    f.render_widget(body, chunks[1]);

    let hint = Line::from(vec![
        Span::raw("  "),
        keybind("↑↓"),
        Span::raw(" navigate · "),
        keybind("Space"),
        Span::raw(" toggle · "),
        keybind("Enter"),
        Span::raw(" edit · "),
        keybind("Esc"),
        Span::raw(" save & close"),
    ]);
    f.render_widget(
        Paragraph::new(hint).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn build_settings_lines(settings: &SettingsState, registers: &[RegisterDef]) -> Vec<Line<'static>> {
    let display_for: BTreeMap<&str, &str> = registers
        .iter()
        .map(|r| {
            (
                r.name.as_str(),
                r.display_name.as_deref().unwrap_or(r.name.as_str()),
            )
        })
        .collect();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_section: Option<&'static str> = None;

    for (idx, item) in settings.items.iter().enumerate() {
        let section = section_for(item);
        if current_section != Some(section) {
            if current_section.is_some() {
                lines.push(Line::from(""));
            }
            push_section_header(&mut lines, section);
            current_section = Some(section);
        }
        lines.push(settings_row(settings, idx, item, &display_for));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "    ───  Ventilation is always selectable",
        Style::default().add_modifier(Modifier::DIM),
    )));
    lines
}

fn section_for(item: &SettingsItem) -> &'static str {
    match item {
        SettingsItem::Host | SettingsItem::Port | SettingsItem::PollInterval => "Connection",
        SettingsItem::Temperature(_) => "Temperatures",
        SettingsItem::ModeHeating | SettingsItem::ModeCooling | SettingsItem::ModeClimate => {
            "Modes"
        }
        SettingsItem::ExhaustFan => "Fans",
    }
}

fn settings_row(
    settings: &SettingsState,
    idx: usize,
    item: &SettingsItem,
    display_for: &BTreeMap<&str, &str>,
) -> Line<'static> {
    let selected = settings.cursor == idx;
    let cursor = if selected { "  ▶ " } else { "    " };
    let label_style = if selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let mut spans = vec![
        Span::styled(
            cursor.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<20}  ", settings_label(item, display_for)),
            label_style,
        ),
    ];
    let editing = settings.editing.as_ref().filter(|b| b.item_index == idx);
    match item {
        SettingsItem::Host | SettingsItem::Port | SettingsItem::PollInterval => {
            if let Some(buf) = editing {
                spans.push(Span::styled(
                    format!("[{}█]", buf.text),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    settings_value_text(settings, item),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if selected && editing.is_none() {
                spans.push(hint_span("   Enter — edit"));
            }
        }
        _ => {
            let on = settings_bool_value(settings, item);
            let (glyph, color) = if on {
                ("[✓]", Color::Green)
            } else {
                ("[ ]", Color::Red)
            };
            spans.push(Span::styled(
                glyph.to_owned(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            if selected {
                spans.push(hint_span("   Space — toggle"));
            }
        }
    }
    Line::from(spans)
}

fn settings_label(item: &SettingsItem, display_for: &BTreeMap<&str, &str>) -> String {
    match item {
        SettingsItem::Host => "Host".into(),
        SettingsItem::Port => "Port".into(),
        SettingsItem::PollInterval => "Poll interval".into(),
        SettingsItem::Temperature(name) => display_for
            .get(name.as_str())
            .copied()
            .unwrap_or(name.as_str())
            .to_owned(),
        SettingsItem::ModeHeating => "Heating".into(),
        SettingsItem::ModeCooling => "Cooling".into(),
        SettingsItem::ModeClimate => "Climate".into(),
        SettingsItem::ExhaustFan => "Exhaust fan".into(),
    }
}

fn settings_value_text(settings: &SettingsState, item: &SettingsItem) -> String {
    match item {
        SettingsItem::Host => settings.draft_modbus.host.clone(),
        SettingsItem::Port => settings.draft_modbus.port.to_string(),
        SettingsItem::PollInterval => format!("{} s", settings.draft_poll.interval_seconds),
        _ => String::new(),
    }
}

fn settings_bool_value(settings: &SettingsState, item: &SettingsItem) -> bool {
    match item {
        SettingsItem::Temperature(name) => settings
            .draft_ui
            .temperatures
            .get(name)
            .copied()
            .unwrap_or(true),
        SettingsItem::ModeHeating => settings.draft_ui.modes.heating,
        SettingsItem::ModeCooling => settings.draft_ui.modes.cooling,
        SettingsItem::ModeClimate => settings.draft_ui.modes.climate,
        SettingsItem::ExhaustFan => settings.draft_ui.exhaust_fan,
        _ => false,
    }
}

// =========================================================================
// Terminal lifecycle
// =========================================================================

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, io::Error> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn def_writable(name: &str) -> RegisterDef {
        RegisterDef {
            name: name.into(),
            kind: crate::domain::RegisterKind::Holding,
            address: 0,
            value_type: crate::domain::RegisterValueType::U16,
            writable: true,
            unit: None,
            group: None,
            display_name: None,
        }
    }

    fn all_visible_ui() -> UiConfig {
        UiConfig {
            temperatures: std::collections::BTreeMap::new(),
            modes: ModeVisibility::default(),
            exhaust_fan: true,
        }
    }

    #[test]
    fn active_controls_includes_only_present_registers() {
        let regs = vec![def_writable("power"), def_writable("temp_setpoint")];
        let controls = active_controls(&regs, &all_visible_ui());
        assert_eq!(controls, vec![ControlId::Power, ControlId::Setpoint]);
    }

    #[test]
    fn active_controls_orders_mode_between_power_and_setpoint() {
        let regs = vec![
            def_writable("temp_setpoint"),
            def_writable("mode_system"),
            def_writable("power"),
        ];
        let controls = active_controls(&regs, &all_visible_ui());
        assert_eq!(
            controls,
            vec![ControlId::Power, ControlId::Mode, ControlId::Setpoint],
        );
    }

    #[test]
    fn active_controls_hides_exhaust_fan_when_disabled() {
        let regs = vec![
            def_writable("power"),
            def_writable("fan_speed_supply"),
            def_writable("fan_speed_exhaust"),
        ];
        let mut ui = all_visible_ui();
        ui.exhaust_fan = false;
        let controls = active_controls(&regs, &ui);
        assert_eq!(controls, vec![ControlId::Power, ControlId::SupplyFan]);
    }

    #[test]
    fn mode_cycle_skips_disabled_options() {
        let vis_all = ModeVisibility::default();
        assert_eq!(
            next_mode_in_cycle(ModeSelection::Ventilation, vis_all),
            ModeSelection::Heating,
        );
        assert_eq!(
            next_mode_in_cycle(ModeSelection::Auto, vis_all),
            ModeSelection::Ventilation,
        );

        let only_climate = ModeVisibility {
            heating: false,
            cooling: false,
            climate: true,
        };
        // Ventilation → (heating, cooling hidden) → Climate
        assert_eq!(
            next_mode_in_cycle(ModeSelection::Ventilation, only_climate),
            ModeSelection::Auto,
        );
        assert_eq!(
            next_mode_in_cycle(ModeSelection::Auto, only_climate),
            ModeSelection::Ventilation,
        );

        let only_vent = ModeVisibility {
            heating: false,
            cooling: false,
            climate: false,
        };
        assert_eq!(
            next_mode_in_cycle(ModeSelection::Ventilation, only_vent),
            ModeSelection::Ventilation,
        );
    }

    #[test]
    fn mode_ui_labels_mirror_mobile_app() {
        assert_eq!(mode_ui_label(ModeSelection::Ventilation), "Ventilation");
        assert_eq!(mode_ui_label(ModeSelection::Heating), "Heating");
        assert_eq!(mode_ui_label(ModeSelection::Cooling), "Cooling");
        assert_eq!(mode_ui_label(ModeSelection::Auto), "Climate");
    }

    #[test]
    fn active_controls_skips_non_writable() {
        let mut reg = def_writable("power");
        reg.writable = false;
        let controls = active_controls(&[reg], &all_visible_ui());
        assert!(controls.is_empty());
    }

    #[test]
    fn apply_step_clamps_to_range() {
        let mut buf = EditBuffer {
            control: ControlId::Setpoint,
            text: "29.5".into(),
        };
        apply_step(&mut buf, 1.0);
        assert_eq!(buf.text, "30.0");
        apply_step(&mut buf, 1.0);
        assert_eq!(buf.text, "30.0");
    }

    #[test]
    fn apply_step_uses_half_degree_step_for_setpoint() {
        let mut buf = EditBuffer {
            control: ControlId::Setpoint,
            text: "22.0".into(),
        };
        apply_step(&mut buf, 1.0);
        assert_eq!(buf.text, "22.5");
    }

    #[test]
    fn apply_step_uses_unit_step_for_fan() {
        let mut buf = EditBuffer {
            control: ControlId::SupplyFan,
            text: "5".into(),
        };
        apply_step(&mut buf, 1.0);
        assert_eq!(buf.text, "6");
        apply_step(&mut buf, -1.0);
        assert_eq!(buf.text, "5");
    }

    #[test]
    fn validate_buffer_rejects_garbage() {
        assert!(validate_buffer(ControlId::Setpoint, "abc").is_err());
        assert!(validate_buffer(ControlId::SupplyFan, "").is_err());
    }

    #[test]
    fn validate_buffer_rejects_out_of_range() {
        assert!(validate_buffer(ControlId::Setpoint, "12.0").is_err());
        assert!(validate_buffer(ControlId::Setpoint, "31.0").is_err());
        assert!(validate_buffer(ControlId::SupplyFan, "11").is_err());
    }

    #[test]
    fn validate_buffer_rejects_zero_fan_setpoint() {
        // Safety: zero fan speed with the heater on burns out the heat
        // exchanger. The TUI must refuse the value at edit time and the
        // CLI must refuse the underlying write — see
        // `app::set_value_refuses_zero_supply_fan`.
        assert!(validate_buffer(ControlId::SupplyFan, "0").is_err());
        assert!(validate_buffer(ControlId::ExhaustFan, "0").is_err());
    }

    #[test]
    fn validate_buffer_accepts_in_range() {
        assert!(validate_buffer(ControlId::Setpoint, "22.5").is_ok());
        assert!(validate_buffer(ControlId::SupplyFan, "1").is_ok());
        assert!(validate_buffer(ControlId::SupplyFan, "10").is_ok());
    }

    #[test]
    fn apply_step_clamps_fan_to_minimum_one() {
        let mut buf = EditBuffer {
            control: ControlId::SupplyFan,
            text: "1".into(),
        };
        apply_step(&mut buf, -1.0);
        assert_eq!(buf.text, "1", "stepping down at the minimum must stay at 1");
    }
}
