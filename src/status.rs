//! Decoded views of well-known GTC registers.
//!
//! The CLI prints raw register values verbatim — this module is the
//! presentation layer the interactive TUI consumes to turn
//! `state_word_0 = 449` into `Power ON, Mode Heating` and
//! `error_code = 0` into `No active errors`.
//!
//! Recognition is by register name. Users who curate their config and
//! keep the canonical names from `default.yml` (`firmware_version`,
//! `state_word_0`, `state_word_1`, `error_code`, `error_code_aux`)
//! get the decoded view; anything else falls through to raw rendering
//! in the caller.
//!
//! Bit-field reference: `docs/Oasis_Registers.md` (input registers
//! `0x01`–`0x06`). Decoder tests pin the layout against the PDF
//! example (`0x2311` = firmware 2.3.17) and against a value observed
//! on a real controller (`0x5100` = firmware 5.1.0, `state_word_0 =
//! 449` decoding to power-on + heating mode).

// Bit-pattern reinterpretation between i16/u16 wire values (in
// find_word) and the bool-rich DeviceState struct are intentional;
// the alternative — packing bits into a u16 mask and accessor methods
// — would obscure how the field maps to the documented register, not
// improve safety.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::struct_excessive_bools
)]

use std::fmt;

use crate::domain::{ModeSelection, RegisterValue, Snapshot};

/// Aggregate decoded status, built from one [`Snapshot`] by
/// [`build_status`]. Every field is independently `Option`-typed so
/// callers can render whichever portions the user has configured.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusView {
    /// Decoded firmware version, e.g. `5.1.0`.
    pub firmware: Option<FirmwareVersion>,
    /// High-level device state — power, mode, schedules, priorities.
    pub state: Option<DeviceState>,
    /// Current operation phase from `state_word_1`.
    pub phase: Option<OperationPhase>,
    /// Active error labels, decoded from `error_code`. Empty when no
    /// register named `error_code` is present in the snapshot **or**
    /// when the register reads zero.
    pub errors: Vec<&'static str>,
    /// Informational / warning labels from `error_code_aux`.
    pub notes: Vec<&'static str>,
    /// Air-out temperature setpoint, in °C. Read from the holding
    /// register registered under the canonical name `temp_setpoint`.
    pub temperature_setpoint: Option<f32>,
    /// Selected operation mode (heating-only / cooling-only / auto).
    /// Decoded from the holding register registered under the
    /// canonical name `mode_system`.
    pub mode_selection: Option<ModeSelection>,
}

/// Decoded firmware version word.
///
/// The hi byte holds major/minor as two nibbles, the lo byte holds
/// the patch number as a plain `u8` (so `0x2311` reads as 2.3.17 per
/// the GTC manual example).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareVersion {
    /// Major version (high nibble of high byte).
    pub major: u8,
    /// Minor version (low nibble of high byte).
    pub minor: u8,
    /// Patch number (the entire low byte, as a decimal integer).
    pub patch: u8,
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Decoded `state_word_0` bit field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceState {
    /// `true` when the unit is currently powered on.
    pub power_on: bool,
    /// The unit is transitioning to the state encoded in [`Self::power_on`].
    pub transitioning: bool,
    /// Heating mode is installed and configured (bit 6).
    pub heating_available: bool,
    /// Cooling mode is installed and configured (bit 7).
    pub cooling_available: bool,
    /// Currently in heating mode (bit 8). Read in conjunction with
    /// [`Self::heating_available`] and [`Self::cooling_available`] to
    /// distinguish "heating because heating is the only option" from
    /// "currently chosen heating over cooling".
    pub active_mode_heating: bool,
    /// A "next 24 hours" timer entry is active.
    pub timer_today: bool,
    /// A "next 7 days" timer entry is active.
    pub timer_week: bool,
    /// Priority sensor for ventilation control.
    pub priority: Priority,
}

impl DeviceState {
    /// Convenience selector — the mode the controller is currently
    /// running in, accounting for what is installed.
    #[must_use]
    pub fn active_mode(&self) -> ActiveMode {
        match (self.heating_available, self.cooling_available) {
            (true, true) => {
                if self.active_mode_heating {
                    ActiveMode::Heating
                } else {
                    ActiveMode::Cooling
                }
            }
            (true, false) => ActiveMode::Heating,
            (false, true) => ActiveMode::Cooling,
            (false, false) => ActiveMode::Ventilation,
        }
    }
}

/// Which thermal mode the controller is currently driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveMode {
    /// Heating is active or is the only installed mode.
    Heating,
    /// Cooling is active or is the only installed mode.
    Cooling,
    /// Neither heating nor cooling is installed — ventilation only.
    Ventilation,
}

impl ActiveMode {
    /// Short human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Heating => "Heating",
            Self::Cooling => "Cooling",
            Self::Ventilation => "Ventilation only",
        }
    }
}

/// Sensor priority for the ventilation regulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// No priority sensor — the unit follows the air-temperature setpoint.
    None,
    /// Humidity sensor takes priority.
    Humidity,
    /// CO₂ sensor takes priority.
    Co2,
    /// Pressure sensor takes priority.
    Pressure,
}

impl Priority {
    /// Short human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "Temperature setpoint",
            Self::Humidity => "Humidity",
            Self::Co2 => "CO₂",
            Self::Pressure => "Pressure",
        }
    }
}

/// Current operation phase from `state_word_1` (bits 0..=4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationPhase {
    /// No transition in progress.
    Idle,
    /// Opening the outside-air damper.
    OpeningDamper,
    /// Pre-heating the heater before starting the fan.
    HeaterPreheating,
    /// Spinning up the fan.
    FanStarting,
    /// "Northern start" — slow warm-up sequence for cold climates.
    NorthernStart,
    /// Fan winding down to a stop.
    FanCoasting,
    /// Closing the outside-air damper.
    ClosingDamper,
    /// Purging the electric heater after shut-off.
    ElectricHeaterPurge,
    /// Opening the hot-water valve.
    OpeningHotWaterValve,
    /// Closing the hot-water valve.
    ClosingHotWaterValve,
    /// Opening the cold-water valve.
    OpeningColdWaterValve,
    /// Closing the cold-water valve.
    ClosingColdWaterValve,
    /// Accelerating the recuperator rotor.
    RotorAccelerating,
    /// Any other phase value — controller running a stage we have
    /// not transcribed yet. The bit-field is preserved for debugging.
    Unknown(u16),
}

impl OperationPhase {
    /// Short human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::OpeningDamper => "Opening damper",
            Self::HeaterPreheating => "Heater preheating",
            Self::FanStarting => "Fan starting",
            Self::NorthernStart => "Northern start",
            Self::FanCoasting => "Fan coasting",
            Self::ClosingDamper => "Closing damper",
            Self::ElectricHeaterPurge => "Electric heater purge",
            Self::OpeningHotWaterValve => "Opening hot-water valve",
            Self::ClosingHotWaterValve => "Closing hot-water valve",
            Self::OpeningColdWaterValve => "Opening cold-water valve",
            Self::ClosingColdWaterValve => "Closing cold-water valve",
            Self::RotorAccelerating => "Rotor accelerating",
            Self::Unknown(_) => "Other phase",
        }
    }
}

/// Bit-to-label map for `error_code` (input register `0x05`).
const ERROR_FLAGS: &[(u8, &str)] = &[
    (0, "Channel sensor T1 fault (open/short)"),
    (1, "Return-water sensor T2 fault (open/short)"),
    (2, "Outdoor sensor T3 fault (open/short)"),
    (3, "Filter 1 pressure sensor fault"),
    (4, "Filter 1 fully clogged"),
    (5, "No coolant in system"),
    (6, "Frost risk: return water below 5 °C"),
    (7, "Frost risk: capillary sensor tripped"),
    (8, "Frost risk: channel air below 5 °C"),
    (9, "Fan 1 pressure sensor fault"),
    (10, "Fan 1 fault"),
    (11, "Fire alarm"),
    (13, "Heater overheat"),
    (14, "Cooler 1 pressure fault"),
    (15, "Cooler 2 pressure fault"),
];

/// Bit-to-label map for `error_code_aux` (input register `0x06`).
/// Most entries here are diagnostic flags rather than hard faults.
const NOTE_FLAGS: &[(u8, &str)] = &[
    (4, "System overheat (setpoint not reached, heat fully off)"),
    (5, "System undercool (setpoint not reached, heat fully on)"),
    (6, "Remote-stop input asserted"),
    (7, "Auto fan-speed reduction enabled"),
    (10, "Northern-start mode active"),
    (11, "Recuperator below 0 °C"),
    (12, "Recuperator above target temperature"),
    (13, "Recuperator icing — preheat active"),
    (14, "Recuperator de-icing fan-speed reduction"),
    (15, "Smooth speed-reduction mode active"),
];

/// Build a [`StatusView`] from a snapshot.
///
/// Looks up each well-known register by name and decodes it; missing
/// registers leave the corresponding fields `None` / empty without
/// failing.
#[must_use]
pub fn build_status(snapshot: &Snapshot) -> StatusView {
    StatusView {
        firmware: find_word(snapshot, "firmware_version").map(decode_firmware),
        state: find_word(snapshot, "state_word_0").map(decode_state),
        phase: find_word(snapshot, "state_word_1").map(decode_phase),
        errors: find_word(snapshot, "error_code")
            .map(|w| decode_flags(w, ERROR_FLAGS))
            .unwrap_or_default(),
        notes: find_word(snapshot, "error_code_aux")
            .map(|w| decode_flags(w, NOTE_FLAGS))
            .unwrap_or_default(),
        temperature_setpoint: find_temperature(snapshot, "temp_setpoint"),
        mode_selection: find_mode(snapshot, "mode_system"),
    }
}

fn find_mode(snapshot: &Snapshot, name: &str) -> Option<ModeSelection> {
    snapshot
        .entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| match e.value {
            RegisterValue::Mode(m) => Some(m),
            _ => None,
        })
}

fn find_temperature(snapshot: &Snapshot, name: &str) -> Option<f32> {
    snapshot
        .entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| match e.value {
            RegisterValue::Temperature(v) => Some(v),
            _ => None,
        })
}

fn find_word(snapshot: &Snapshot, name: &str) -> Option<u16> {
    snapshot
        .entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| match e.value {
            RegisterValue::U16(w) => Some(w),
            RegisterValue::I16(i) => Some(i as u16),
            RegisterValue::Percent(p) => Some(p),
            _ => None,
        })
}

fn decode_firmware(word: u16) -> FirmwareVersion {
    let hi = (word >> 8) as u8;
    let lo = (word & 0xFF) as u8;
    FirmwareVersion {
        major: (hi >> 4) & 0x0F,
        minor: hi & 0x0F,
        patch: lo,
    }
}

fn decode_state(word: u16) -> DeviceState {
    DeviceState {
        power_on: word & (1 << 0) != 0,
        transitioning: word & (1 << 1) != 0,
        heating_available: word & (1 << 6) != 0,
        cooling_available: word & (1 << 7) != 0,
        active_mode_heating: word & (1 << 8) != 0,
        timer_today: word & (1 << 9) != 0,
        timer_week: word & (1 << 10) != 0,
        priority: match (word >> 11) & 0b11 {
            1 => Priority::Humidity,
            2 => Priority::Co2,
            3 => Priority::Pressure,
            _ => Priority::None,
        },
    }
}

fn decode_phase(word: u16) -> OperationPhase {
    match word & 0x1F {
        0 => OperationPhase::Idle,
        1 => OperationPhase::OpeningDamper,
        2 => OperationPhase::HeaterPreheating,
        3 => OperationPhase::FanStarting,
        4 => OperationPhase::NorthernStart,
        5 => OperationPhase::FanCoasting,
        6 => OperationPhase::ClosingDamper,
        7 => OperationPhase::ElectricHeaterPurge,
        8 => OperationPhase::OpeningHotWaterValve,
        9 => OperationPhase::ClosingHotWaterValve,
        10 => OperationPhase::OpeningColdWaterValve,
        11 => OperationPhase::ClosingColdWaterValve,
        12 => OperationPhase::RotorAccelerating,
        other => OperationPhase::Unknown(other),
    }
}

fn decode_flags(word: u16, table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table
        .iter()
        .filter(|(bit, _)| word & (1u16 << bit) != 0)
        .map(|(_, label)| *label)
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::domain::{RegisterValue, Snapshot, SnapshotEntry};

    #[test]
    fn firmware_decoding_matches_pdf_example_2_3_17() {
        let fw = decode_firmware(0x2311);
        assert_eq!(fw.major, 2);
        assert_eq!(fw.minor, 3);
        assert_eq!(fw.patch, 17);
        assert_eq!(fw.to_string(), "2.3.17");
    }

    #[test]
    fn firmware_decoding_matches_observed_5_1_0() {
        let fw = decode_firmware(0x5100);
        assert_eq!(fw.to_string(), "5.1.0");
    }

    #[test]
    fn state_decoding_matches_observed_449() {
        let s = decode_state(449);
        assert!(s.power_on, "bit 0 = power");
        assert!(!s.transitioning);
        assert!(s.heating_available);
        assert!(s.cooling_available);
        assert!(s.active_mode_heating);
        assert!(!s.timer_today);
        assert!(!s.timer_week);
        assert_eq!(s.priority, Priority::None);
        assert_eq!(s.active_mode(), ActiveMode::Heating);
    }

    #[test]
    fn state_off_when_bit_0_clear() {
        let s = decode_state(0);
        assert!(!s.power_on);
        assert_eq!(s.active_mode(), ActiveMode::Ventilation);
    }

    #[test]
    fn state_priority_decoding() {
        let humidity = decode_state(1 | (1 << 11));
        assert_eq!(humidity.priority, Priority::Humidity);
        let co2 = decode_state(1 | (2 << 11));
        assert_eq!(co2.priority, Priority::Co2);
        let pressure = decode_state(1 | (3 << 11));
        assert_eq!(pressure.priority, Priority::Pressure);
    }

    #[test]
    fn state_active_mode_falls_back_to_installed() {
        let heat_only = DeviceState {
            power_on: true,
            transitioning: false,
            heating_available: true,
            cooling_available: false,
            active_mode_heating: false,
            timer_today: false,
            timer_week: false,
            priority: Priority::None,
        };
        assert_eq!(heat_only.active_mode(), ActiveMode::Heating);

        let cool_only = DeviceState {
            heating_available: false,
            cooling_available: true,
            active_mode_heating: true,
            ..heat_only
        };
        assert_eq!(cool_only.active_mode(), ActiveMode::Cooling);
    }

    #[test]
    fn phase_idle_is_zero() {
        assert_eq!(decode_phase(0), OperationPhase::Idle);
    }

    #[test]
    fn phase_known_values_decode_individually() {
        let pairs: &[(u16, OperationPhase)] = &[
            (1, OperationPhase::OpeningDamper),
            (3, OperationPhase::FanStarting),
            (6, OperationPhase::ClosingDamper),
            (12, OperationPhase::RotorAccelerating),
        ];
        for (word, expected) in pairs {
            assert_eq!(decode_phase(*word), *expected);
        }
    }

    #[test]
    fn phase_unknown_carries_raw_bits() {
        // 31 = 0x1F, the maximum value that fits in bits 0..=4 but is
        // not enumerated in the documented phase list.
        assert_eq!(decode_phase(31), OperationPhase::Unknown(31));
    }

    #[test]
    fn phase_only_looks_at_low_five_bits() {
        // Bits 5..=15 are documented as reserved.
        assert_eq!(decode_phase(0xFFE0 | 3), OperationPhase::FanStarting);
    }

    #[test]
    fn errors_empty_for_zero() {
        assert!(decode_flags(0, ERROR_FLAGS).is_empty());
    }

    #[test]
    fn errors_collect_multiple_set_bits() {
        // Bit 0 (T1 fault) and bit 11 (fire alarm).
        let word = 1 | (1 << 11);
        let labels = decode_flags(word, ERROR_FLAGS);
        assert_eq!(labels.len(), 2);
        assert!(labels.iter().any(|l| l.contains("T1")));
        assert!(labels.iter().any(|l| l.contains("Fire")));
    }

    #[test]
    fn notes_decode_real_device_value_16() {
        // The live controller returned error_code_aux = 16 (bit 4):
        // "system overheat" — setpoint not reached even with heat off.
        let labels = decode_flags(16, NOTE_FLAGS);
        assert_eq!(labels.len(), 1);
        assert!(labels[0].contains("overheat"));
    }

    #[test]
    fn build_status_empty_snapshot() {
        let view = build_status(&Snapshot::new());
        assert!(view.firmware.is_none());
        assert!(view.state.is_none());
        assert!(view.phase.is_none());
        assert!(view.errors.is_empty());
        assert!(view.notes.is_empty());
        assert!(view.temperature_setpoint.is_none());
        assert!(view.mode_selection.is_none());
    }

    #[test]
    fn build_status_picks_up_mode_selection() {
        let snap = Snapshot {
            entries: vec![SnapshotEntry {
                name: "mode_system".into(),
                value: RegisterValue::Mode(ModeSelection::Auto),
                unit: None,
            }],
        };
        let view = build_status(&snap);
        assert_eq!(view.mode_selection, Some(ModeSelection::Auto));
    }

    #[test]
    fn build_status_picks_up_temperature_setpoint() {
        let snap = Snapshot {
            entries: vec![SnapshotEntry {
                name: "temp_setpoint".into(),
                value: RegisterValue::Temperature(22.5),
                unit: Some("°C".into()),
            }],
        };
        let view = build_status(&snap);
        let target = view.temperature_setpoint.unwrap();
        assert!((target - 22.5).abs() < 1e-6);
    }

    #[test]
    fn build_status_ignores_non_temperature_setpoint_values() {
        let snap = Snapshot {
            entries: vec![SnapshotEntry {
                name: "temp_setpoint".into(),
                value: RegisterValue::U16(225),
                unit: None,
            }],
        };
        let view = build_status(&snap);
        // A user mis-typed the register as `u16` instead of
        // `temperature_x10`; rather than silently mis-scale by 10, we
        // refuse to surface the value at all and let them notice.
        assert!(view.temperature_setpoint.is_none());
    }

    #[test]
    fn build_status_recognises_well_known_names() {
        let snap = Snapshot {
            entries: vec![
                SnapshotEntry {
                    name: "firmware_version".into(),
                    value: RegisterValue::U16(0x5100),
                    unit: None,
                },
                SnapshotEntry {
                    name: "state_word_0".into(),
                    value: RegisterValue::U16(449),
                    unit: None,
                },
                SnapshotEntry {
                    name: "state_word_1".into(),
                    value: RegisterValue::U16(0),
                    unit: None,
                },
                SnapshotEntry {
                    name: "error_code".into(),
                    value: RegisterValue::U16(0),
                    unit: None,
                },
                SnapshotEntry {
                    name: "error_code_aux".into(),
                    value: RegisterValue::U16(16),
                    unit: None,
                },
            ],
        };
        let view = build_status(&snap);
        assert_eq!(view.firmware.unwrap().to_string(), "5.1.0");
        assert!(view.state.unwrap().power_on);
        assert_eq!(view.phase.unwrap(), OperationPhase::Idle);
        assert!(view.errors.is_empty());
        assert_eq!(view.notes.len(), 1);
    }

    #[test]
    fn build_status_ignores_unrelated_entries() {
        let snap = Snapshot {
            entries: vec![SnapshotEntry {
                name: "supply_air_temp".into(),
                value: RegisterValue::Temperature(21.5),
                unit: Some("°C".into()),
            }],
        };
        let view = build_status(&snap);
        assert!(view.firmware.is_none());
        assert!(view.state.is_none());
    }
}
