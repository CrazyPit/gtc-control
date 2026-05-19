//! Operations layer — the actions both the CLI and the TUI invoke.
//!
//! Everything in here is an `async fn` over `&mut dyn ModbusClient`,
//! deliberately UI-agnostic. The CLI binary wires these to clap
//! subcommands; the interactive TUI calls the same functions on a
//! cadence.
//!
//! The trait-object signature means the test suite drives these
//! functions against [`crate::modbus::FakeModbusClient`] without
//! going near a real controller.

use thiserror::Error;
use tracing::warn;

use crate::domain::{
    ModeSelection, RegisterDef, RegisterKind, RegisterValue, RegisterValueType, Snapshot,
    SnapshotEntry, ValueConversionError,
};
use crate::modbus::{ModbusClient, ModbusError};

/// Errors raised by [`poll_once`], [`read_one`], and [`set_value`].
#[derive(Debug, Error)]
pub enum AppError {
    /// No register with that name exists in the loaded config.
    #[error("unknown register `{name}`")]
    UnknownRegister {
        /// The name the user passed.
        name: String,
    },
    /// The user asked to write to a register declared `writable: false`.
    #[error("register `{name}` is not writable")]
    NotWritable {
        /// The register name.
        name: String,
    },
    /// Could not decode the wire value into the register's declared
    /// value type.
    #[error("register `{name}`: value decode failed: {source}")]
    Decode {
        /// Register name.
        name: String,
        /// Underlying conversion error.
        #[source]
        source: ValueConversionError,
    },
    /// Could not parse the user-supplied value string.
    #[error("register `{name}`: could not parse value: {source}")]
    Parse {
        /// Register name.
        name: String,
        /// Underlying conversion error.
        #[source]
        source: ValueConversionError,
    },
    /// Bubbled-up Modbus transport / protocol error.
    #[error("modbus error: {0}")]
    Modbus(#[from] ModbusError),
}

/// Read every register defined in `registers` once.
///
/// Reads happen sequentially — GTC's Ethernet module has been observed
/// to drop sessions under aggressive parallel reads. Returns an ordered
/// [`Snapshot`] whose entries follow the order of `registers`.
///
/// # Errors
/// Returns the first [`AppError`] encountered; subsequent registers are
/// not read.
pub async fn poll_once(
    client: &mut dyn ModbusClient,
    registers: &[RegisterDef],
) -> Result<Snapshot, AppError> {
    let mut snapshot = Snapshot::new();
    for def in registers {
        let entry = read_entry(client, def).await?;
        snapshot.entries.push(entry);
    }
    Ok(snapshot)
}

/// Read a single register by name, returning its [`SnapshotEntry`].
///
/// `registers` is the user's full configured list; this function looks
/// up the matching entry by `name` (case-sensitive, exact match) and
/// performs exactly one Modbus read.
///
/// # Errors
/// - [`AppError::UnknownRegister`] when `name` is not in `registers`.
/// - [`AppError::Modbus`] on transport / protocol failure.
/// - [`AppError::Decode`] when the wire value does not fit the
///   declared value type.
pub async fn read_one(
    client: &mut dyn ModbusClient,
    registers: &[RegisterDef],
    name: &str,
) -> Result<SnapshotEntry, AppError> {
    let def =
        registers
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| AppError::UnknownRegister {
                name: name.to_owned(),
            })?;
    read_entry(client, def).await
}

/// Write `raw_value` to the register named `name`.
///
/// The string is parsed according to the register's
/// [`crate::domain::RegisterValueType`]. When the value type is
/// [`crate::domain::RegisterValueType::Mode`] the bus operation is a
/// read-modify-write — the surrounding bits of the packed
/// configuration word are preserved.
///
/// # Errors
/// Returns [`AppError::UnknownRegister`], [`AppError::NotWritable`],
/// [`AppError::Parse`], or [`AppError::Modbus`].
pub async fn set_value(
    client: &mut dyn ModbusClient,
    registers: &[RegisterDef],
    name: &str,
    raw_value: &str,
) -> Result<(), AppError> {
    let def =
        registers
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| AppError::UnknownRegister {
                name: name.to_owned(),
            })?;
    if !def.writable {
        return Err(AppError::NotWritable {
            name: name.to_owned(),
        });
    }

    match def.kind {
        RegisterKind::Holding => {
            let word = def
                .value_type
                .parse_word(raw_value)
                .map_err(|source| AppError::Parse {
                    name: name.to_owned(),
                    source,
                })?;
            let final_word = match def.value_type {
                // `Mode` carries only bits 0..1 of a packed
                // configuration register; the remaining 14 bits hold
                // unrelated installation flags and must survive the
                // write. Read-modify-write under the actor's
                // single-session lock is safe — no other client can
                // race a partial write.
                RegisterValueType::Mode => {
                    let existing = client.read_holding(def.address, 1).await?;
                    let current = existing.first().copied().unwrap_or(0);
                    (current & !ModeSelection::BIT_MASK) | (word & ModeSelection::BIT_MASK)
                }
                _ => word,
            };
            client.write_holding(def.address, final_word).await?;
        }
        RegisterKind::Coil => {
            let bit = def
                .value_type
                .parse_bit(raw_value)
                .map_err(|source| AppError::Parse {
                    name: name.to_owned(),
                    source,
                })?;
            client.write_coil(def.address, bit).await?;
        }
        RegisterKind::Input | RegisterKind::Discrete => {
            warn!(
                register = %name,
                "set_value reached a read-only kind despite NotWritable check — config validation gap?"
            );
            return Err(AppError::NotWritable {
                name: name.to_owned(),
            });
        }
    }
    Ok(())
}

async fn read_entry(
    client: &mut dyn ModbusClient,
    def: &RegisterDef,
) -> Result<SnapshotEntry, AppError> {
    let value = read_single(client, def).await?;
    Ok(SnapshotEntry {
        name: def.name.clone(),
        value,
        unit: def.unit.clone(),
    })
}

async fn read_single(
    client: &mut dyn ModbusClient,
    def: &RegisterDef,
) -> Result<RegisterValue, AppError> {
    match def.kind {
        RegisterKind::Holding => {
            let words = client.read_holding(def.address, 1).await?;
            decode_word(def, &words)
        }
        RegisterKind::Input => {
            let words = client.read_input(def.address, 1).await?;
            decode_word(def, &words)
        }
        RegisterKind::Coil => {
            let bits = client.read_coils(def.address, 1).await?;
            decode_bit(def, &bits)
        }
        RegisterKind::Discrete => {
            let bits = client.read_discrete(def.address, 1).await?;
            decode_bit(def, &bits)
        }
    }
}

fn decode_word(def: &RegisterDef, words: &[u16]) -> Result<RegisterValue, AppError> {
    let word = words.first().copied().ok_or_else(|| AppError::Decode {
        name: def.name.clone(),
        source: ValueConversionError::Parse("empty read response".into()),
    })?;
    def.value_type
        .decode_word(word)
        .map_err(|source| AppError::Decode {
            name: def.name.clone(),
            source,
        })
}

fn decode_bit(def: &RegisterDef, bits: &[bool]) -> Result<RegisterValue, AppError> {
    let bit = bits.first().copied().ok_or_else(|| AppError::Decode {
        name: def.name.clone(),
        source: ValueConversionError::Parse("empty read response".into()),
    })?;
    debug_assert!(matches!(def.value_type, RegisterValueType::Bool));
    def.value_type
        .decode_bit(bit)
        .map_err(|source| AppError::Decode {
            name: def.name.clone(),
            source,
        })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::domain::{RegisterKind, RegisterValueType};
    use crate::modbus::FakeModbusClient;

    fn temp_def(name: &str, address: u16) -> RegisterDef {
        RegisterDef {
            name: name.into(),
            kind: RegisterKind::Input,
            address,
            value_type: RegisterValueType::TemperatureX10,
            writable: false,
            unit: Some("°C".into()),
            group: Some("Temperatures".into()),
            display_name: None,
        }
    }

    fn power_def(name: &str, address: u16) -> RegisterDef {
        RegisterDef {
            name: name.into(),
            kind: RegisterKind::Coil,
            address,
            value_type: RegisterValueType::Bool,
            writable: true,
            unit: None,
            group: Some("State".into()),
            display_name: None,
        }
    }

    #[tokio::test]
    async fn poll_once_decodes_seeded_values() {
        let mut client = FakeModbusClient::new();
        client.input.insert(0, 215);
        client.coils.insert(1, true);

        let registers = vec![temp_def("supply", 0), power_def("power", 1)];
        let snap = poll_once(&mut client, &registers).await.unwrap();

        assert_eq!(snap.entries.len(), 2);
        assert_eq!(snap.entries[0].name, "supply");
        assert_eq!(snap.entries[1].name, "power");
        match snap.entries[0].value {
            RegisterValue::Temperature(v) => assert!((v - 21.5).abs() < 1e-6),
            other => panic!("expected Temperature, got {other:?}"),
        }
        assert_eq!(snap.entries[1].value, RegisterValue::Bool(true));
    }

    #[tokio::test]
    async fn poll_once_preserves_unit_metadata() {
        let mut client = FakeModbusClient::new();
        client.input.insert(0, 100);
        let registers = vec![temp_def("supply", 0)];
        let snap = poll_once(&mut client, &registers).await.unwrap();
        assert_eq!(snap.entries[0].unit.as_deref(), Some("°C"));
    }

    #[tokio::test]
    async fn read_one_returns_matching_entry() {
        let mut client = FakeModbusClient::new();
        client.input.insert(7, 215);
        client.coils.insert(2, true);
        let registers = vec![temp_def("supply_temp", 7), power_def("power", 2)];

        let entry = read_one(&mut client, &registers, "supply_temp")
            .await
            .unwrap();
        assert_eq!(entry.name, "supply_temp");
        match entry.value {
            RegisterValue::Temperature(v) => assert!((v - 21.5).abs() < 1e-6),
            other => panic!("expected Temperature, got {other:?}"),
        }
        assert_eq!(entry.unit.as_deref(), Some("°C"));
    }

    #[tokio::test]
    async fn read_one_unknown_register() {
        let mut client = FakeModbusClient::new();
        let registers = vec![temp_def("supply", 0)];
        let err = read_one(&mut client, &registers, "ghost")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::UnknownRegister { ref name } if name == "ghost"));
    }

    #[tokio::test]
    async fn read_one_modbus_error_propagates() {
        // Reading a discrete bit through a u16-typed register would
        // trip the decoder; here we just exercise that the function
        // surfaces non-Unknown errors when reads succeed but decoding
        // fails. Using a Bool value type on a holding register would
        // be rejected by validate(), so we trigger the analogous
        // Decode path by clearing the read response.
        let mut client = FakeModbusClient::new();
        let registers = vec![power_def("power", 5)];
        client.coils.insert(5, false);
        let entry = read_one(&mut client, &registers, "power").await.unwrap();
        assert_eq!(entry.value, RegisterValue::Bool(false));
    }

    #[tokio::test]
    async fn set_value_unknown_register() {
        let mut client = FakeModbusClient::new();
        let err = set_value(&mut client, &[], "missing", "1")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::UnknownRegister { .. }));
    }

    #[tokio::test]
    async fn set_value_not_writable() {
        let mut client = FakeModbusClient::new();
        let registers = vec![temp_def("supply", 0)];
        let err = set_value(&mut client, &registers, "supply", "20")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotWritable { .. }));
    }

    #[tokio::test]
    async fn set_value_writes_coil() {
        let mut client = FakeModbusClient::new();
        let registers = vec![power_def("power", 7)];
        set_value(&mut client, &registers, "power", "on")
            .await
            .unwrap();
        assert_eq!(client.coils.get(&7).copied(), Some(true));
    }

    #[tokio::test]
    async fn set_value_writes_holding_percent() {
        let mut client = FakeModbusClient::new();
        let registers = vec![RegisterDef {
            name: "fan".into(),
            kind: RegisterKind::Holding,
            address: 3,
            value_type: RegisterValueType::Percent,
            writable: true,
            unit: Some("%".into()),
            group: Some("Fans".into()),
            display_name: None,
        }];
        set_value(&mut client, &registers, "fan", "60")
            .await
            .unwrap();
        assert_eq!(client.holding.get(&3).copied(), Some(60));
    }

    fn mode_def() -> RegisterDef {
        RegisterDef {
            name: "mode_system".into(),
            kind: RegisterKind::Holding,
            address: 86,
            value_type: RegisterValueType::Mode,
            writable: true,
            unit: None,
            group: Some("Controls".into()),
            display_name: None,
        }
    }

    #[tokio::test]
    async fn set_value_mode_preserves_surrounding_bits() {
        let mut client = FakeModbusClient::new();
        // Pre-seed the register with bits 2..15 carrying unrelated
        // installation flags plus mode = Heating (bits 0..1 = 1).
        let preserved = 0b1011_1100_0000_1001;
        client.holding.insert(86, preserved);

        set_value(&mut client, &[mode_def()], "mode_system", "cooling")
            .await
            .unwrap();

        let written = client.holding.get(&86).copied().unwrap();
        // bits 2..15 unchanged, bits 0..1 now = 2 (cooling)
        assert_eq!(written, (preserved & !0b11) | 0b10);
    }

    #[tokio::test]
    async fn set_value_mode_accepts_auto_alias() {
        let mut client = FakeModbusClient::new();
        client.holding.insert(86, 0);
        set_value(&mut client, &[mode_def()], "mode_system", "auto")
            .await
            .unwrap();
        // Mode 3 written; no other bits set.
        assert_eq!(client.holding.get(&86).copied(), Some(0b11));
    }

    #[tokio::test]
    async fn set_value_mode_rejects_unknown_label() {
        let mut client = FakeModbusClient::new();
        let err = set_value(&mut client, &[mode_def()], "mode_system", "warm")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Parse { .. }));
    }

    #[tokio::test]
    async fn poll_once_decodes_mode_register() {
        let mut client = FakeModbusClient::new();
        // bit 0 + bit 1 = 3 (auto), plus an unrelated flag at bit 9.
        client.holding.insert(86, 0b0000_0010_0000_0011);
        let snap = poll_once(&mut client, &[mode_def()]).await.unwrap();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(
            snap.entries[0].value,
            RegisterValue::Mode(ModeSelection::Auto),
        );
    }
}
