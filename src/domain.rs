//! Domain types.
//!
//! Pure, I/O-free data structures used throughout `GTC_Control`. This
//! module owns the vocabulary of Modbus registers (kind, value type,
//! definition, decoded value) and the [`Snapshot`] type returned by
//! a poll. No async, no network, no platform code lives here — these
//! types are trivially testable.

// Modbus words encode signed and unsigned 16-bit integers by bit
// pattern; the `u16 as i16` / `i16 as u16` conversions throughout this
// module are intentional reinterpretations, not arithmetic casts. The
// single `f32 as i16` (in `parse_word` for `TemperatureX10`) is guarded
// by an explicit `i16::MIN..=i16::MAX` range check immediately before
// the cast. The `i32 as f32` (in `decode_word` for `TemperatureX10`)
// can never overflow because the i32 originates from an i16.
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::match_same_arms
)]

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Which Modbus address space a register lives in.
///
/// Holdings and coils are read/write; inputs and discrete inputs are
/// read-only. The `writable: true` flag in [`RegisterDef`] is rejected at
/// config-validation time for read-only kinds — see
/// [`RegisterDef::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisterKind {
    /// 16-bit read/write register (function codes 0x03 read, 0x06 write).
    Holding,
    /// 16-bit read-only register (function code 0x04 read).
    Input,
    /// 1-bit read/write register (function codes 0x01 read, 0x05 write).
    Coil,
    /// 1-bit read-only register (function code 0x02 read).
    Discrete,
}

impl RegisterKind {
    /// Whether this kind permits writes at all (independent of the
    /// per-register `writable` flag).
    #[must_use]
    pub const fn is_writable_kind(self) -> bool {
        matches!(self, Self::Holding | Self::Coil)
    }

    /// Whether this kind is bit-valued (`true`) or word-valued (`false`).
    #[must_use]
    pub const fn is_bit(self) -> bool {
        matches!(self, Self::Coil | Self::Discrete)
    }
}

/// How the raw Modbus word should be interpreted on the wire.
///
/// `Bool` is only valid for bit-valued kinds ([`RegisterKind::Coil`] and
/// [`RegisterKind::Discrete`]). All other variants are word-valued and
/// only valid for [`RegisterKind::Holding`] and [`RegisterKind::Input`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisterValueType {
    /// Unsigned 16-bit integer, taken as-is.
    U16,
    /// Signed 16-bit integer (the register's `u16` reinterpreted as `i16`).
    I16,
    /// Single-bit boolean. Only valid for coils and discrete inputs.
    Bool,
    /// Signed 16-bit integer scaled by 10 — display as `value / 10.0` °C.
    TemperatureX10,
    /// Unsigned 16-bit integer in `0..=100`. Out-of-range writes are
    /// rejected; reads outside the range surface as
    /// [`RegisterValue::Percent`] regardless and are logged.
    Percent,
    /// System operation mode encoded in the lower two bits of a
    /// packed configuration word (`Dev_Keys_2`, holding `0x57`). The
    /// remaining 14 bits carry unrelated installation flags and must
    /// be preserved by any write — see
    /// [`crate::app::set_value`] for the read-modify-write path.
    Mode,
}

/// Selectable system operation mode encoded in `Dev_Keys_2` bits 0..1.
///
/// Encoding mirrors the four-option mode picker in the GTC mobile
/// app: `0` = ventilation only (no thermal stage), `1` = heating only,
/// `2` = cooling only, `3` = climate control (controller picks heat
/// or cool from setpoint). The PDF documents `1`/`2`/`3`; value `0`
/// is undocumented but is what the mobile app writes when the user
/// picks the "Ventilation" option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSelection {
    /// Ventilation only — bits 0..1 == `0`. The unit moves air but
    /// the heating and cooling stages stay off regardless of the
    /// setpoint.
    Ventilation,
    /// Heating only — bits 0..1 == `1`.
    Heating,
    /// Cooling only — bits 0..1 == `2`.
    Cooling,
    /// Climate control — bits 0..1 == `3`. Controller picks heat or
    /// cool based on the air-out setpoint.
    Auto,
}

impl ModeSelection {
    /// Mask of the bits this value occupies inside `Dev_Keys_2`.
    pub const BIT_MASK: u16 = 0b11;

    /// Decode the low two bits of `word` into a mode selection.
    #[must_use]
    pub const fn from_word(word: u16) -> Self {
        match word & Self::BIT_MASK {
            0 => Self::Ventilation,
            1 => Self::Heating,
            2 => Self::Cooling,
            _ => Self::Auto,
        }
    }

    /// Encode this mode as a 2-bit value suitable for OR-ing into
    /// the packed register.
    #[must_use]
    pub const fn to_bits(self) -> u16 {
        match self {
            Self::Ventilation => 0,
            Self::Heating => 1,
            Self::Cooling => 2,
            Self::Auto => 3,
        }
    }

    /// Short lower-case English label used by [`Self::parse`] and the
    /// CLI display path (`"ventilation"`, `"heating"`, `"cooling"`,
    /// `"auto"`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ventilation => "ventilation",
            Self::Heating => "heating",
            Self::Cooling => "cooling",
            Self::Auto => "auto",
        }
    }

    /// Parse a CLI / actor-command string into a [`ModeSelection`].
    ///
    /// Accepts the canonical labels and the raw register integers
    /// `0`/`1`/`2`/`3`, case-insensitively. Aliases: `vent` →
    /// ventilation, `heat` → heating, `cool` → cooling,
    /// `automatic` / `climate` → auto.
    ///
    /// # Errors
    /// Returns [`ValueConversionError::Parse`] for any other input.
    pub fn parse(raw: &str) -> Result<Self, ValueConversionError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ventilation" | "vent" | "0" => Ok(Self::Ventilation),
            "heating" | "heat" | "1" => Ok(Self::Heating),
            "cooling" | "cool" | "2" => Ok(Self::Cooling),
            "auto" | "automatic" | "climate" | "3" => Ok(Self::Auto),
            other => Err(ValueConversionError::Parse(format!(
                "expected ventilation/heating/cooling/auto, got `{other}`"
            ))),
        }
    }
}

/// A user-supplied register definition.
///
/// Loaded verbatim from the bundled `config/default.yml`. Validate with
/// [`RegisterDef::validate`] before use — the YAML schema cannot express
/// the kind ↔ value-type compatibility rules on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDef {
    /// Stable identifier — what the CLI passes to `set <name> <value>`.
    /// Conventionally lower-case ASCII, no whitespace. Uniqueness across
    /// the register list is checked at config load.
    pub name: String,
    /// Which Modbus address space the register lives in.
    pub kind: RegisterKind,
    /// Zero-based Modbus address.
    pub address: u16,
    /// How to decode the raw word(s) for display and parse strings on
    /// `set`.
    #[serde(rename = "value")]
    pub value_type: RegisterValueType,
    /// Whether the CLI accepts `set` against this register. Always
    /// rejected for [`RegisterKind::Input`] / [`RegisterKind::Discrete`].
    #[serde(default)]
    pub writable: bool,
    /// Free-form display string appended after the value (`"°C"`, `"%"`,
    /// …). Optional; `None` renders the raw decoded value.
    #[serde(default)]
    pub unit: Option<String>,
    /// Optional section label used by sectioned interfaces (the
    /// register-list output, the TUI). Registers with the same `group`
    /// render under a shared heading; registers without one are placed
    /// in an "Other" catch-all section. Has no effect on Modbus I/O.
    #[serde(default)]
    pub group: Option<String>,
    /// Optional human-friendly label used by the interactive TUI.
    /// When `None`, the TUI falls back to [`Self::name`]. Has no
    /// effect on Modbus I/O.
    #[serde(default)]
    pub display_name: Option<String>,
}

impl RegisterDef {
    /// Validate kind/value-type/writable consistency.
    ///
    /// # Errors
    /// Returns [`RegisterDefError::WritableOnReadOnlyKind`] if `writable`
    /// is set on an input or discrete-input register, and
    /// [`RegisterDefError::ValueTypeKindMismatch`] if the value type is
    /// incompatible with the kind (e.g. `Bool` on a holding register).
    pub fn validate(&self) -> Result<(), RegisterDefError> {
        if self.writable && !self.kind.is_writable_kind() {
            return Err(RegisterDefError::WritableOnReadOnlyKind {
                name: self.name.clone(),
                kind: self.kind,
            });
        }
        let value_is_bit = matches!(self.value_type, RegisterValueType::Bool);
        if value_is_bit != self.kind.is_bit() {
            return Err(RegisterDefError::ValueTypeKindMismatch {
                name: self.name.clone(),
                kind: self.kind,
                value_type: self.value_type,
            });
        }
        Ok(())
    }
}

/// Errors surfaced by [`RegisterDef::validate`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegisterDefError {
    /// `writable: true` on a kind that has no write function code.
    #[error("register `{name}`: kind `{kind:?}` is read-only and cannot be `writable: true`")]
    WritableOnReadOnlyKind {
        /// Register name that triggered the error.
        name: String,
        /// The offending kind.
        kind: RegisterKind,
    },
    /// Value type does not match the bit-ness of the kind.
    #[error("register `{name}`: value type `{value_type:?}` is incompatible with kind `{kind:?}`")]
    ValueTypeKindMismatch {
        /// Register name that triggered the error.
        name: String,
        /// The kind under which the value type is invalid.
        kind: RegisterKind,
        /// The offending value type.
        value_type: RegisterValueType,
    },
}

/// A decoded register value, ready for display or further computation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegisterValue {
    /// Raw 16-bit unsigned.
    U16(u16),
    /// 16-bit signed (after reinterpretation of the wire word).
    I16(i16),
    /// Coil / discrete-input bit.
    Bool(bool),
    /// Temperature in degrees Celsius (1 decimal place precision).
    Temperature(f32),
    /// Percent, `0..=100` under valid devices.
    Percent(u16),
    /// Decoded mode selection from `Dev_Keys_2` bits 0..1.
    Mode(ModeSelection),
}

impl fmt::Display for RegisterValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::U16(v) => write!(f, "{v}"),
            Self::I16(v) => write!(f, "{v}"),
            Self::Bool(true) => f.write_str("on"),
            Self::Bool(false) => f.write_str("off"),
            Self::Temperature(v) => write!(f, "{v:.1}"),
            Self::Percent(v) => write!(f, "{v}"),
            Self::Mode(m) => f.write_str(m.label()),
        }
    }
}

impl RegisterValueType {
    /// Decode a single Modbus word into a typed [`RegisterValue`].
    ///
    /// Only valid for word-valued types — calling this on
    /// [`RegisterValueType::Bool`] returns
    /// [`ValueConversionError::BitTypeNotConvertible`].
    ///
    /// # Errors
    /// See [`ValueConversionError`].
    pub fn decode_word(self, word: u16) -> Result<RegisterValue, ValueConversionError> {
        match self {
            Self::U16 => Ok(RegisterValue::U16(word)),
            Self::I16 => Ok(RegisterValue::I16(word as i16)),
            Self::TemperatureX10 => {
                let scaled = i32::from(word as i16);
                Ok(RegisterValue::Temperature((scaled as f32) / 10.0))
            }
            Self::Percent => Ok(RegisterValue::Percent(word)),
            Self::Mode => Ok(RegisterValue::Mode(ModeSelection::from_word(word))),
            Self::Bool => Err(ValueConversionError::BitTypeNotConvertible),
        }
    }

    /// Decode a single coil/discrete bit into a typed [`RegisterValue`].
    ///
    /// # Errors
    /// Returns [`ValueConversionError::WordTypeNotConvertible`] for
    /// word-valued types.
    pub fn decode_bit(self, bit: bool) -> Result<RegisterValue, ValueConversionError> {
        match self {
            Self::Bool => Ok(RegisterValue::Bool(bit)),
            _ => Err(ValueConversionError::WordTypeNotConvertible),
        }
    }

    /// Parse a CLI string into the raw `u16` to put on the wire (for
    /// word-valued types).
    ///
    /// # Errors
    /// Returns [`ValueConversionError::Parse`] for malformed input,
    /// [`ValueConversionError::OutOfRange`] for values outside the
    /// representable range, and [`ValueConversionError::BitTypeNotConvertible`]
    /// when called on [`RegisterValueType::Bool`].
    pub fn parse_word(self, raw: &str) -> Result<u16, ValueConversionError> {
        let trimmed = raw.trim();
        match self {
            Self::U16 => trimmed
                .parse::<u16>()
                .map_err(|e| ValueConversionError::Parse(e.to_string())),
            Self::I16 => {
                let v: i16 = trimmed
                    .parse::<i16>()
                    .map_err(|e| ValueConversionError::Parse(e.to_string()))?;
                Ok(v as u16)
            }
            Self::TemperatureX10 => {
                let v: f32 = trimmed
                    .parse::<f32>()
                    .map_err(|e| ValueConversionError::Parse(e.to_string()))?;
                let scaled = (v * 10.0).round();
                if !(f32::from(i16::MIN)..=f32::from(i16::MAX)).contains(&scaled) {
                    return Err(ValueConversionError::OutOfRange {
                        value: raw.to_owned(),
                        reason: "scaled value exceeds i16",
                    });
                }
                Ok(scaled as i16 as u16)
            }
            Self::Percent => {
                let v: u16 = trimmed
                    .parse::<u16>()
                    .map_err(|e| ValueConversionError::Parse(e.to_string()))?;
                if v > 100 {
                    return Err(ValueConversionError::OutOfRange {
                        value: raw.to_owned(),
                        reason: "percent must be in 0..=100",
                    });
                }
                Ok(v)
            }
            Self::Mode => ModeSelection::parse(raw).map(ModeSelection::to_bits),
            Self::Bool => Err(ValueConversionError::BitTypeNotConvertible),
        }
    }

    /// Parse a CLI string into a bit value (for [`RegisterValueType::Bool`]).
    ///
    /// Accepts `true/false`, `on/off`, `1/0`, case-insensitively.
    ///
    /// # Errors
    /// Returns [`ValueConversionError::Parse`] for any unrecognised
    /// spelling and [`ValueConversionError::WordTypeNotConvertible`] when
    /// called on a word-valued type.
    pub fn parse_bit(self, raw: &str) -> Result<bool, ValueConversionError> {
        if !matches!(self, Self::Bool) {
            return Err(ValueConversionError::WordTypeNotConvertible);
        }
        match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "1" => Ok(true),
            "false" | "off" | "0" => Ok(false),
            other => Err(ValueConversionError::Parse(format!(
                "expected true/false/on/off/1/0, got `{other}`"
            ))),
        }
    }
}

/// Errors raised by the conversion helpers on [`RegisterValueType`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValueConversionError {
    /// The string did not parse as the expected numeric or boolean form.
    #[error("could not parse value: {0}")]
    Parse(String),
    /// The parsed value lies outside the register's allowed range.
    #[error("value `{value}` out of range: {reason}")]
    OutOfRange {
        /// The original input that was rejected.
        value: String,
        /// Human-readable reason (e.g. "percent must be in 0..=100").
        reason: &'static str,
    },
    /// Tried to decode a word for a bit-valued type.
    #[error("bit-valued type cannot decode from a 16-bit word")]
    BitTypeNotConvertible,
    /// Tried to decode a bit for a word-valued type.
    #[error("word-valued type cannot decode from a single bit")]
    WordTypeNotConvertible,
}

/// One entry in a [`Snapshot`] — a register name paired with its decoded
/// value and the display unit lifted from the [`RegisterDef`].
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotEntry {
    /// Register name (as declared in the config).
    pub name: String,
    /// Decoded value.
    pub value: RegisterValue,
    /// Optional unit string (already stripped of leading whitespace).
    pub unit: Option<String>,
}

/// The result of a single poll across the register map.
///
/// Entries appear in the same order as the registers in the config — that
/// makes the CLI output stable and diff-friendly.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// Ordered list of decoded register values.
    pub entries: Vec<SnapshotEntry>,
}

impl Snapshot {
    /// Construct an empty snapshot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::float_cmp
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn validate_rejects_writable_on_input() {
        let def = RegisterDef {
            name: "temp".into(),
            kind: RegisterKind::Input,
            address: 0,
            value_type: RegisterValueType::I16,
            writable: true,
            unit: None,
            group: None,
            display_name: None,
        };
        let err = def.validate().unwrap_err();
        assert!(matches!(
            err,
            RegisterDefError::WritableOnReadOnlyKind { .. }
        ));
    }

    #[test]
    fn validate_rejects_bool_on_holding() {
        let def = RegisterDef {
            name: "power".into(),
            kind: RegisterKind::Holding,
            address: 0,
            value_type: RegisterValueType::Bool,
            writable: true,
            unit: None,
            group: None,
            display_name: None,
        };
        let err = def.validate().unwrap_err();
        assert!(matches!(
            err,
            RegisterDefError::ValueTypeKindMismatch { .. }
        ));
    }

    #[test]
    fn validate_accepts_bool_on_coil() {
        let def = RegisterDef {
            name: "power".into(),
            kind: RegisterKind::Coil,
            address: 0,
            value_type: RegisterValueType::Bool,
            writable: true,
            unit: None,
            group: None,
            display_name: None,
        };
        assert!(def.validate().is_ok());
    }

    #[test]
    fn decode_temperature_x10_rounds_to_one_decimal() {
        let v = RegisterValueType::TemperatureX10.decode_word(215).unwrap();
        match v {
            RegisterValue::Temperature(f) => assert!((f - 21.5).abs() < 1e-6),
            other => panic!("expected Temperature, got {other:?}"),
        }
    }

    #[test]
    fn decode_temperature_x10_handles_negative() {
        let raw: u16 = (-50_i16) as u16;
        let v = RegisterValueType::TemperatureX10.decode_word(raw).unwrap();
        match v {
            RegisterValue::Temperature(f) => assert!((f - (-5.0)).abs() < 1e-6),
            other => panic!("expected Temperature, got {other:?}"),
        }
    }

    #[test]
    fn parse_percent_rejects_above_100() {
        let err = RegisterValueType::Percent.parse_word("150").unwrap_err();
        assert!(matches!(err, ValueConversionError::OutOfRange { .. }));
    }

    #[test]
    fn parse_bool_accepts_synonyms() {
        for v in ["true", "On", "1"] {
            assert!(RegisterValueType::Bool.parse_bit(v).unwrap());
        }
        for v in ["false", "OFF", "0"] {
            assert!(!RegisterValueType::Bool.parse_bit(v).unwrap());
        }
    }

    #[test]
    fn parse_bool_rejects_garbage() {
        let err = RegisterValueType::Bool.parse_bit("maybe").unwrap_err();
        assert!(matches!(err, ValueConversionError::Parse(_)));
    }

    #[test]
    fn mode_decoding_only_reads_low_two_bits() {
        // Surrounding bits are unrelated configuration flags; the
        // decoder must mask them off.
        assert_eq!(
            ModeSelection::from_word(0b1010_1010_1010_1000),
            ModeSelection::Ventilation,
        );
        assert_eq!(
            ModeSelection::from_word(0b1010_1010_1010_1001),
            ModeSelection::Heating,
        );
        assert_eq!(
            ModeSelection::from_word(0b0000_0000_0000_0010),
            ModeSelection::Cooling,
        );
        assert_eq!(
            ModeSelection::from_word(0b1111_1111_1111_1111),
            ModeSelection::Auto,
        );
    }

    #[test]
    fn mode_to_bits_matches_firmware_encoding() {
        assert_eq!(ModeSelection::Ventilation.to_bits(), 0);
        assert_eq!(ModeSelection::Heating.to_bits(), 1);
        assert_eq!(ModeSelection::Cooling.to_bits(), 2);
        assert_eq!(ModeSelection::Auto.to_bits(), 3);
    }

    #[test]
    fn mode_parse_accepts_labels_and_numbers() {
        assert_eq!(
            ModeSelection::parse("ventilation"),
            Ok(ModeSelection::Ventilation),
        );
        assert_eq!(ModeSelection::parse("vent"), Ok(ModeSelection::Ventilation));
        assert_eq!(ModeSelection::parse("0"), Ok(ModeSelection::Ventilation));
        assert_eq!(ModeSelection::parse("heating"), Ok(ModeSelection::Heating));
        assert_eq!(ModeSelection::parse("HEAT"), Ok(ModeSelection::Heating));
        assert_eq!(ModeSelection::parse("Cooling"), Ok(ModeSelection::Cooling));
        assert_eq!(ModeSelection::parse(" auto "), Ok(ModeSelection::Auto));
        assert_eq!(ModeSelection::parse("automatic"), Ok(ModeSelection::Auto));
        assert_eq!(ModeSelection::parse("climate"), Ok(ModeSelection::Auto));
        assert_eq!(ModeSelection::parse("1"), Ok(ModeSelection::Heating));
        assert_eq!(ModeSelection::parse("3"), Ok(ModeSelection::Auto));
    }

    #[test]
    fn mode_parse_rejects_garbage() {
        let err = ModeSelection::parse("warm").unwrap_err();
        assert!(matches!(err, ValueConversionError::Parse(_)));
    }

    #[test]
    fn parse_word_mode_returns_register_bits() {
        let bits = RegisterValueType::Mode.parse_word("cooling").unwrap();
        assert_eq!(bits, 2);
    }

    #[test]
    fn decode_word_mode_extracts_selection() {
        let v = RegisterValueType::Mode
            .decode_word(0b1010_1010_1010_1011)
            .unwrap();
        assert_eq!(v, RegisterValue::Mode(ModeSelection::Auto));
    }

    #[test]
    fn mode_display_uses_label() {
        let s = RegisterValue::Mode(ModeSelection::Heating).to_string();
        assert_eq!(s, "heating");
    }

    #[test]
    fn register_def_round_trips_through_yaml() {
        let original = RegisterDef {
            name: "supply".into(),
            kind: RegisterKind::Input,
            address: 7,
            value_type: RegisterValueType::TemperatureX10,
            writable: false,
            unit: Some("°C".into()),
            group: Some("Temperatures".into()),
            display_name: Some("Supply air".into()),
        };
        let yaml = serde_norway::to_string(&original).expect("yaml serialisation");
        let restored: RegisterDef = serde_norway::from_str(&yaml).expect("yaml parse");
        assert_eq!(original, restored);
    }

    #[test]
    fn register_def_optional_fields_default_to_none() {
        let yaml = r"
name: power
kind: coil
address: 0
value: bool
writable: true
";
        let parsed: RegisterDef = serde_norway::from_str(yaml).expect("yaml parse");
        assert_eq!(parsed.group, None);
        assert_eq!(parsed.unit, None);
        assert_eq!(parsed.display_name, None);
    }

    proptest! {
        #[test]
        fn u16_round_trips_through_decode(w: u16) {
            let decoded = RegisterValueType::U16.decode_word(w).unwrap();
            prop_assert_eq!(decoded, RegisterValue::U16(w));
        }

        #[test]
        fn i16_round_trips_through_decode(v: i16) {
            let decoded = RegisterValueType::I16.decode_word(v as u16).unwrap();
            prop_assert_eq!(decoded, RegisterValue::I16(v));
        }

        #[test]
        fn temperature_round_trips_through_decode(v in -3276_i16..=3276_i16) {
            let scaled = v as u16;
            let decoded = RegisterValueType::TemperatureX10.decode_word(scaled).unwrap();
            match decoded {
                RegisterValue::Temperature(f) => {
                    let back = (f * 10.0).round() as i16;
                    prop_assert_eq!(back, v);
                }
                other => prop_assert!(false, "expected Temperature, got {:?}", other),
            }
        }
    }
}
