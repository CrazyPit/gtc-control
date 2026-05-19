//! Configuration: bundled register catalogue + user-editable preferences.
//!
//! The register addresses, value types, and group labels live in
//! `config/default.yml` and are compiled into the binary — users do
//! not touch them.
//!
//! The user-tunable subset (Modbus endpoint, polling cadence, and the
//! `ui` visibility toggles) lives in `~/.gtc-control/config.yml`.
//! [`load_or_init`] materialises the file from the bundled defaults
//! on first launch; the interactive Settings screen rewrites it via
//! [`save_user_config`] on close.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{RegisterDef, RegisterDefError};

/// Bundled configuration. The single source of truth for register
/// addresses, value types, group labels, and the factory-default
/// connection + UI preferences.
const BUNDLED_CONFIG_YAML: &str = include_str!("../config/default.yml");

/// Header written at the top of `~/.gtc-control/config.yml` so a
/// human opening the file in an editor knows what it is.
const USER_CONFIG_HEADER: &str = "\
# GTC_Control — user settings.
#
# Rewritten by the interactive Settings screen (press `s` in the main
# view, edit, press `Esc` to save). Edits made in an external editor
# are picked up on the next launch. Comments are not preserved across
# Settings-screen saves; if you add them, expect them to be stripped.
#
# The register catalogue (addresses, value types) is baked into the
# binary — only the fields you see below are user-tunable.

";

/// Directory inside `$HOME` where `GTC_Control` stores its user
/// settings.
const CONFIG_DIR_NAME: &str = ".gtc-control";

/// File name for the user-editable settings file.
const CONFIG_FILE_NAME: &str = "config.yml";

/// Modbus TCP endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModbusConfig {
    /// Hostname or IPv4 address of the GTC Ethernet module.
    pub host: String,
    /// TCP port — Modbus standard is 502.
    pub port: u16,
    /// Modbus unit/slave identifier — typically `1` for GTC.
    pub unit_id: u8,
    /// Per-request read timeout in milliseconds.
    pub timeout_ms: u64,
}

/// Polling cadence for the interactive view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollConfig {
    /// Interval between full register sweeps, in seconds.
    pub interval_seconds: u64,
}

/// UI visibility preferences. Each entry hides or shows a row in the
/// interactive view without affecting the underlying register
/// catalogue or Modbus traffic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Whether each temperature sensor row is visible. Missing keys
    /// (e.g. a sensor added after the user file was last written)
    /// default to `true` — see [`Config::temperature_visible`].
    #[serde(default)]
    pub temperatures: BTreeMap<String, bool>,
    /// Which thermal modes the user can pick from the Mode cycle.
    /// `Ventilation` is always selectable and is therefore not
    /// represented here.
    #[serde(default)]
    pub modes: ModeVisibility,
    /// Whether the exhaust-fan rows (current speed + setpoint) are
    /// visible. The supply fan is always shown.
    #[serde(default = "default_true")]
    pub exhaust_fan: bool,
}

/// Per-mode visibility toggles. `Ventilation` is intentionally
/// missing — it is always selectable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModeVisibility {
    /// Heating-only mode shows up in the Mode cycle.
    #[serde(default = "default_true")]
    pub heating: bool,
    /// Cooling-only mode shows up in the Mode cycle.
    #[serde(default = "default_true")]
    pub cooling: bool,
    /// Climate (firmware "automatic") mode shows up in the Mode cycle.
    #[serde(default = "default_true")]
    pub climate: bool,
}

impl Default for ModeVisibility {
    fn default() -> Self {
        Self {
            heating: true,
            cooling: true,
            climate: true,
        }
    }
}

impl Default for UiConfig {
    /// Default visibility is "everything shown" — `exhaust_fan = true`,
    /// all modes selectable, no explicit temperature entries (which
    /// defer to [`Config::temperature_visible`]'s default-true rule).
    fn default() -> Self {
        Self {
            temperatures: BTreeMap::new(),
            modes: ModeVisibility::default(),
            exhaust_fan: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Resolved configuration the rest of the app sees.
#[derive(Debug, Clone)]
pub struct Config {
    /// Modbus endpoint settings.
    pub modbus: ModbusConfig,
    /// Polling cadence.
    pub poll: PollConfig,
    /// UI visibility preferences.
    pub ui: UiConfig,
    /// Register catalogue.
    pub registers: Vec<RegisterDef>,
}

impl Config {
    /// Look up a register definition by name.
    #[must_use]
    pub fn register(&self, name: &str) -> Option<&RegisterDef> {
        self.registers.iter().find(|r| r.name == name)
    }

    /// Whether a given temperature sensor row should be drawn in the
    /// interactive view. Defaults to `true` for register names not
    /// listed in [`UiConfig::temperatures`].
    #[must_use]
    pub fn temperature_visible(&self, register_name: &str) -> bool {
        self.ui
            .temperatures
            .get(register_name)
            .copied()
            .unwrap_or(true)
    }
}

/// The schema of the bundled `default.yml`. Carries both user-tunable
/// fields and the register catalogue, so the single file documents
/// every default in one place.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BundledConfig {
    modbus: ModbusConfig,
    poll: PollConfig,
    #[serde(default)]
    ui: UiConfig,
    registers: Vec<RegisterDef>,
}

/// The schema of `~/.gtc-control/config.yml`. A subset of
/// [`BundledConfig`] — everything except the register catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserConfig {
    modbus: ModbusConfig,
    poll: PollConfig,
    #[serde(default)]
    ui: UiConfig,
}

/// Errors raised while resolving, parsing, or validating the config.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// `dirs::home_dir()` returned `None` — the host has no `$HOME`.
    #[error("could not determine the user's home directory")]
    NoHome,
    /// Filesystem operation failed (create dir, read file, write
    /// file, rename temp file).
    #[error("filesystem error at {path}: {source}")]
    Io {
        /// Path the operation was attempted against.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The bundled YAML failed to parse — only ever surfaces in
    /// development, since the YAML ships with the binary.
    #[error("invalid bundled config YAML: {0}")]
    BundledParse(serde_norway::Error),
    /// The user file at `~/.gtc-control/config.yml` is not valid
    /// YAML or does not match the user-config schema.
    #[error("invalid YAML in {path}: {source}")]
    UserParse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_norway::Error,
    },
    /// Could not serialise the in-memory user config to YAML.
    #[error("failed to serialise user config for {path}: {source}")]
    Serialize {
        /// Target path of the (attempted) write.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_norway::Error,
    },
    /// One register failed [`RegisterDef::validate`].
    #[error("invalid register definition: {source}")]
    InvalidRegister {
        /// Underlying validation error.
        #[source]
        source: RegisterDefError,
    },
    /// Two or more registers share the same `name`.
    #[error("duplicate register name `{name}` in bundled config")]
    DuplicateRegisterName {
        /// The duplicate name.
        name: String,
    },
}

/// Resolve the path to the user's config file, creating the parent
/// directory if needed.
///
/// # Errors
/// Returns [`ConfigError::NoHome`] if `$HOME` cannot be determined,
/// or [`ConfigError::Io`] if the config directory cannot be created.
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let home = dirs::home_dir().ok_or(ConfigError::NoHome)?;
    let dir = home.join(CONFIG_DIR_NAME);
    fs::create_dir_all(&dir).map_err(|source| ConfigError::Io {
        path: dir.clone(),
        source,
    })?;
    Ok(dir.join(CONFIG_FILE_NAME))
}

/// Load the resolved configuration.
///
/// Materialises `~/.gtc-control/config.yml` from the bundled defaults
/// on first launch, then merges those user-set fields with the
/// bundled register catalogue.
///
/// # Errors
/// Surfaces filesystem, parse, and validation errors via
/// [`ConfigError`].
pub fn load_or_init() -> Result<Config, ConfigError> {
    let bundled = parse_bundled()?;
    validate_registers(&bundled.registers)?;

    let path = config_path()?;
    if !path.exists() {
        write_user_config(
            &path,
            &UserConfig {
                modbus: bundled.modbus.clone(),
                poll: bundled.poll.clone(),
                ui: bundled.ui.clone(),
            },
        )?;
    }
    let user = parse_user_config(&path)?;

    Ok(Config {
        modbus: user.modbus,
        poll: user.poll,
        ui: user.ui,
        registers: bundled.registers,
    })
}

/// Persist the user-tunable subset of `cfg` to
/// `~/.gtc-control/config.yml`. Atomic — writes to a sibling temp
/// file, then renames.
///
/// # Errors
/// Surfaces [`ConfigError::NoHome`], [`ConfigError::Io`], or
/// [`ConfigError::Serialize`] depending on which step fails.
pub fn save_user_config(cfg: &Config) -> Result<(), ConfigError> {
    let path = config_path()?;
    write_user_config(
        &path,
        &UserConfig {
            modbus: cfg.modbus.clone(),
            poll: cfg.poll.clone(),
            ui: cfg.ui.clone(),
        },
    )
}

fn parse_bundled() -> Result<BundledConfig, ConfigError> {
    serde_norway::from_str(BUNDLED_CONFIG_YAML).map_err(ConfigError::BundledParse)
}

fn parse_user_config(path: &Path) -> Result<UserConfig, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_norway::from_str(&raw).map_err(|source| ConfigError::UserParse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_user_config(path: &Path, cfg: &UserConfig) -> Result<(), ConfigError> {
    let body = serde_norway::to_string(cfg).map_err(|source| ConfigError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    let mut content = String::with_capacity(USER_CONFIG_HEADER.len() + body.len());
    content.push_str(USER_CONFIG_HEADER);
    content.push_str(&body);

    let tmp_path = path.with_extension("yml.tmp");
    fs::write(&tmp_path, content).map_err(|source| ConfigError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    fs::rename(&tmp_path, path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn validate_registers(registers: &[RegisterDef]) -> Result<(), ConfigError> {
    let mut seen = HashSet::with_capacity(registers.len());
    for reg in registers {
        reg.validate()
            .map_err(|source| ConfigError::InvalidRegister { source })?;
        if !seen.insert(reg.name.as_str()) {
            return Err(ConfigError::DuplicateRegisterName {
                name: reg.name.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bundled_parses_and_register_set_is_valid() {
        let bundled = parse_bundled().expect("bundled YAML must parse");
        validate_registers(&bundled.registers).expect("bundled registers must validate");
        for required in [
            "firmware_version",
            "state_word_0",
            "power",
            "mode_system",
            "temp_setpoint",
        ] {
            assert!(
                bundled.registers.iter().any(|r| r.name == required),
                "bundled config missing required register `{required}`"
            );
        }
    }

    #[test]
    fn bundled_ui_defaults_to_all_visible() {
        let bundled = parse_bundled().expect("parse");
        assert!(bundled.ui.exhaust_fan);
        assert!(bundled.ui.modes.heating);
        assert!(bundled.ui.modes.cooling);
        assert!(bundled.ui.modes.climate);
        for temp in [
            "supply_air_temp",
            "return_water_temp",
            "outdoor_temp",
            "room_temp",
            "recuperator_outlet_temp",
        ] {
            assert_eq!(
                bundled.ui.temperatures.get(temp).copied(),
                Some(true),
                "default ui.temperatures must list `{temp}` as visible"
            );
        }
    }

    #[test]
    fn user_config_round_trips_through_yaml() {
        let original = UserConfig {
            modbus: ModbusConfig {
                host: "10.0.0.5".into(),
                port: 1502,
                unit_id: 2,
                timeout_ms: 2000,
            },
            poll: PollConfig {
                interval_seconds: 10,
            },
            ui: UiConfig {
                temperatures: BTreeMap::from([
                    ("supply_air_temp".to_owned(), false),
                    ("outdoor_temp".to_owned(), true),
                ]),
                modes: ModeVisibility {
                    heating: false,
                    cooling: true,
                    climate: true,
                },
                exhaust_fan: false,
            },
        };
        let yaml = serde_norway::to_string(&original).expect("serialise");
        let restored: UserConfig = serde_norway::from_str(&yaml).expect("parse");
        assert_eq!(restored.modbus.host, original.modbus.host);
        assert_eq!(restored.poll.interval_seconds, 10);
        assert!(!restored.ui.exhaust_fan);
        assert!(!restored.ui.modes.heating);
        assert!(restored.ui.modes.cooling);
        assert_eq!(
            restored.ui.temperatures.get("supply_air_temp").copied(),
            Some(false),
        );
    }

    #[test]
    fn user_config_missing_ui_defaults_to_visible() {
        let yaml = "
modbus:
  host: 192.168.169.102
  port: 502
  unit_id: 1
  timeout_ms: 1500
poll:
  interval_seconds: 5
";
        let user: UserConfig = serde_norway::from_str(yaml).expect("parse");
        assert!(user.ui.exhaust_fan);
        assert!(user.ui.modes.heating);
        assert!(user.ui.temperatures.is_empty());
    }

    #[test]
    fn temperature_visible_defaults_to_true_for_unlisted() {
        let cfg = Config {
            modbus: ModbusConfig {
                host: "x".into(),
                port: 502,
                unit_id: 1,
                timeout_ms: 1000,
            },
            poll: PollConfig {
                interval_seconds: 5,
            },
            ui: UiConfig {
                temperatures: BTreeMap::from([("supply_air_temp".to_owned(), false)]),
                modes: ModeVisibility::default(),
                exhaust_fan: true,
            },
            registers: Vec::new(),
        };
        assert!(!cfg.temperature_visible("supply_air_temp"));
        assert!(cfg.temperature_visible("outdoor_temp"));
        assert!(cfg.temperature_visible("future_sensor_added_later"));
    }

    #[test]
    fn duplicate_register_name_is_rejected() {
        use crate::domain::{RegisterKind, RegisterValueType};
        let registers = vec![
            RegisterDef {
                name: "power".into(),
                kind: RegisterKind::Coil,
                address: 0,
                value_type: RegisterValueType::Bool,
                writable: true,
                unit: None,
                group: None,
                display_name: None,
            },
            RegisterDef {
                name: "power".into(),
                kind: RegisterKind::Coil,
                address: 1,
                value_type: RegisterValueType::Bool,
                writable: true,
                unit: None,
                group: None,
                display_name: None,
            },
        ];
        let err = validate_registers(&registers).unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateRegisterName { .. }));
    }

    #[test]
    fn invalid_register_propagates() {
        use crate::domain::{RegisterKind, RegisterValueType};
        let registers = vec![RegisterDef {
            name: "bad".into(),
            kind: RegisterKind::Input,
            address: 0,
            value_type: RegisterValueType::I16,
            writable: true,
            unit: None,
            group: None,
            display_name: None,
        }];
        let err = validate_registers(&registers).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidRegister { .. }));
    }
}
