//! Binary entry point for `GTC_Control`.
//!
//! Owns the CLI surface (clap derive) and the tokio runtime. All
//! operations come from the [`gtc_control::app`] layer; this file is
//! just glue: clap dispatch, stdout formatting, exit codes.

use std::io::Write as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::{error, info};

use gtc_control::app::{AppError, poll_once, read_one, set_value};
use gtc_control::config::{self, Config};
use gtc_control::modbus::TcpClient;
use gtc_control::tui::{self, TuiError};
use gtc_control::{format_register_list, format_single_entry, format_snapshot};

/// Command-line interface for `GTC_Control`.
#[derive(Debug, Parser)]
#[command(name = "GTC_Control", version, about, long_about = None)]
struct Cli {
    /// Subcommand to run. With no subcommand, launches the
    /// full-screen interactive status view.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read every configured register once and print the grouped
    /// snapshot.
    Poll,
    /// Read a single register by name and print just its value.
    Read {
        /// Register name as declared in the bundled register
        /// catalogue.
        name: String,
    },
    /// Write a value to a writable register. Parsed according to the
    /// register's declared value type.
    Set {
        /// Register name as declared in the bundled register
        /// catalogue.
        name: String,
        /// Value to write (e.g. `1` for power, `22.5` for setpoint).
        value: String,
    },
    /// Print the bundled register catalogue. No Modbus traffic.
    List,
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            error!(%err, "failed to start tokio runtime");
            return ExitCode::from(2);
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "command failed");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Tui(#[from] TuiError),
    #[error("I/O error writing to stdout: {0}")]
    Io(#[from] std::io::Error),
}

async fn run(cli: Cli) -> Result<(), CliError> {
    let cfg = config::load_or_init()?;
    info!(
        host = %cfg.modbus.host,
        port = cfg.modbus.port,
        unit_id = cfg.modbus.unit_id,
        registers = cfg.registers.len(),
        "loaded config"
    );

    match cli.command {
        None => Ok(tui::run(cfg).await?),
        Some(Command::Poll) => cmd_poll(&cfg).await,
        Some(Command::Read { name }) => cmd_read(&cfg, &name).await,
        Some(Command::Set { name, value }) => cmd_set(&cfg, &name, &value).await,
        Some(Command::List) => cmd_list(&cfg),
    }
}

async fn cmd_poll(cfg: &Config) -> Result<(), CliError> {
    let mut client = TcpClient::new(cfg.modbus.clone());
    let snapshot = poll_once(&mut client, &cfg.registers).await?;
    output(&format_snapshot(&snapshot, &cfg.registers))?;
    Ok(())
}

async fn cmd_read(cfg: &Config, name: &str) -> Result<(), CliError> {
    let mut client = TcpClient::new(cfg.modbus.clone());
    let entry = read_one(&mut client, &cfg.registers, name).await?;
    output(&format_single_entry(&entry))?;
    Ok(())
}

async fn cmd_set(cfg: &Config, name: &str, value: &str) -> Result<(), CliError> {
    let mut client = TcpClient::new(cfg.modbus.clone());
    set_value(&mut client, &cfg.registers, name, value).await?;
    output(&format!("ok: {name} = {value}\n"))?;
    Ok(())
}

fn cmd_list(cfg: &Config) -> Result<(), CliError> {
    output(&format_register_list(&cfg.registers))?;
    Ok(())
}

#[allow(clippy::print_stdout)] // single dedicated stdout sink, per CLAUDE.md.
fn output(text: &str) -> Result<(), std::io::Error> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(text.as_bytes())?;
    handle.flush()
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gtc_control=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
