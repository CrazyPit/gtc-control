//! `gtc_control` — a Modbus TCP controller for GTC ventilation units.
//!
//! Layering (outer depends on inner, never the reverse):
//!
//! ```text
//! main (CLI)   ─┐
//! tui          ─┴─▶  app  ─▶  modbus
//!                            config
//!                            domain
//! ```
//!
//! - [`domain`] — pure value types: register kind, value type,
//!   definitions, decoded values, snapshots.
//! - [`config`] — bundled register catalogue (`config/default.yml`,
//!   compiled in via `include_str!`) merged with the user-editable
//!   `~/.gtc-control/config.yml` for the Modbus endpoint, poll
//!   cadence, and UI visibility preferences.
//! - [`modbus`] — `ModbusClient` trait, `tokio-modbus` TCP implementation,
//!   and an in-memory fake.
//! - [`app`] — operations: `poll_once`, `read_one`, `set_value`. The
//!   shared "command API" that the CLI binary and the TUI call
//!   uniformly.
//! - [`status`] — decoders that turn well-known register reads into
//!   the friendly view the TUI renders.
//! - [`tui`] — the full-screen interactive view (ratatui + crossterm).
//!
//! The free-standing functions [`format_snapshot`],
//! [`format_single_entry`], and [`format_register_list`] are
//! presentation helpers used by the CLI subcommands.

pub mod app;
pub mod config;
pub mod domain;
pub mod modbus;
pub mod status;
pub mod tui;

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::domain::{RegisterDef, RegisterKind, RegisterValueType, Snapshot, SnapshotEntry};

/// Section label rendered for snapshot entries / registers whose
/// `group` field is `None`.
const UNGROUPED_LABEL: &str = "Other";

/// Format a [`Snapshot`] as a stable, sectioned block suitable for
/// printing to stdout.
///
/// Entries are bucketed by the `group` of their matching
/// [`RegisterDef`] (looked up by name in `registers`). Sections appear
/// in first-encounter order — i.e. the order in which the user wrote
/// them in `config.yml` — which keeps the output diff-friendly across
/// invocations.
///
/// Inside each section, entries appear in the snapshot's order. Names
/// are left-aligned across the whole snapshot so columns line up even
/// across sections.
///
/// An empty snapshot yields an empty string.
#[must_use]
pub fn format_snapshot(snapshot: &Snapshot, registers: &[RegisterDef]) -> String {
    if snapshot.entries.is_empty() {
        return String::new();
    }

    let width = snapshot
        .entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0);

    let group_of: BTreeMap<&str, &str> = registers
        .iter()
        .map(|r| {
            (
                r.name.as_str(),
                r.group.as_deref().unwrap_or(UNGROUPED_LABEL),
            )
        })
        .collect();

    let mut sections: Vec<(String, Vec<&SnapshotEntry>)> = Vec::new();
    for entry in &snapshot.entries {
        let group = group_of
            .get(entry.name.as_str())
            .copied()
            .unwrap_or(UNGROUPED_LABEL)
            .to_owned();
        if let Some(existing) = sections.iter_mut().find(|(g, _)| g == &group) {
            existing.1.push(entry);
        } else {
            sections.push((group, vec![entry]));
        }
    }

    let mut out = String::new();
    for (i, (group, entries)) in sections.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "{group}");
        for entry in entries {
            let _ = write!(
                out,
                "  {:<width$}  {}",
                entry.name,
                entry.value,
                width = width
            );
            if let Some(unit) = entry.unit.as_deref() {
                let _ = write!(out, " {unit}");
            }
            out.push('\n');
        }
    }
    out
}

/// Format a single [`SnapshotEntry`] as `value unit`, with no register
/// name — used by `GTC_Control read <name>` where the user has just
/// asked for one value and the name is implied.
///
/// The trailing newline is included so callers can write the result
/// directly to stdout.
#[must_use]
pub fn format_single_entry(entry: &SnapshotEntry) -> String {
    let mut out = entry.value.to_string();
    if let Some(unit) = entry.unit.as_deref() {
        out.push(' ');
        out.push_str(unit);
    }
    out.push('\n');
    out
}

/// Format the configured register catalogue as a column-aligned table
/// suitable for `GTC_Control list`.
///
/// Columns: `NAME`, `KIND`, `ADDR` (hex + decimal), `TYPE`, `UNIT`,
/// `WRITE` (`R` or `RW`), `GROUP`. Lists are sorted first by group
/// (using the original config order for stability), then by the order
/// the user wrote them. Returns an empty string for an empty list.
#[must_use]
pub fn format_register_list(registers: &[RegisterDef]) -> String {
    if registers.is_empty() {
        return String::new();
    }

    let rows: Vec<Row> = registers.iter().map(Row::from_def).collect();

    let widths = ColumnWidths::compute(&rows);
    let mut out = String::new();
    widths.write_header(&mut out);
    for row in &rows {
        widths.write_row(&mut out, row);
    }
    out
}

struct Row {
    name: String,
    kind: &'static str,
    address: String,
    value_type: &'static str,
    unit: String,
    write: &'static str,
    group: String,
}

impl Row {
    fn from_def(def: &RegisterDef) -> Self {
        Self {
            name: def.name.clone(),
            kind: kind_label(def.kind),
            address: format!("0x{:02X} ({})", def.address, def.address),
            value_type: value_type_label(def.value_type),
            unit: def.unit.clone().unwrap_or_default(),
            write: if def.writable { "RW" } else { "R" },
            group: def
                .group
                .clone()
                .unwrap_or_else(|| UNGROUPED_LABEL.to_owned()),
        }
    }
}

struct ColumnWidths {
    name: usize,
    kind: usize,
    address: usize,
    value_type: usize,
    unit: usize,
    write: usize,
}

impl ColumnWidths {
    fn compute(rows: &[Row]) -> Self {
        let mut w = Self {
            name: "NAME".len(),
            kind: "KIND".len(),
            address: "ADDR".len(),
            value_type: "TYPE".len(),
            unit: "UNIT".len(),
            write: "ACCESS".len(),
        };
        for r in rows {
            w.name = w.name.max(r.name.len());
            w.kind = w.kind.max(r.kind.len());
            w.address = w.address.max(r.address.len());
            w.value_type = w.value_type.max(r.value_type.len());
            w.unit = w.unit.max(r.unit.len());
            w.write = w.write.max(r.write.len());
        }
        w
    }

    fn write_header(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "{:<nw$}  {:<kw$}  {:<aw$}  {:<tw$}  {:<uw$}  {:<ww$}  GROUP",
            "NAME",
            "KIND",
            "ADDR",
            "TYPE",
            "UNIT",
            "ACCESS",
            nw = self.name,
            kw = self.kind,
            aw = self.address,
            tw = self.value_type,
            uw = self.unit,
            ww = self.write,
        );
    }

    fn write_row(&self, out: &mut String, row: &Row) {
        let _ = writeln!(
            out,
            "{:<nw$}  {:<kw$}  {:<aw$}  {:<tw$}  {:<uw$}  {:<ww$}  {}",
            row.name,
            row.kind,
            row.address,
            row.value_type,
            row.unit,
            row.write,
            row.group,
            nw = self.name,
            kw = self.kind,
            aw = self.address,
            tw = self.value_type,
            uw = self.unit,
            ww = self.write,
        );
    }
}

fn kind_label(kind: RegisterKind) -> &'static str {
    match kind {
        RegisterKind::Holding => "holding",
        RegisterKind::Input => "input",
        RegisterKind::Coil => "coil",
        RegisterKind::Discrete => "discrete",
    }
}

fn value_type_label(vt: RegisterValueType) -> &'static str {
    match vt {
        RegisterValueType::U16 => "u16",
        RegisterValueType::I16 => "i16",
        RegisterValueType::Bool => "bool",
        RegisterValueType::TemperatureX10 => "temperature_x10",
        RegisterValueType::Percent => "percent",
        RegisterValueType::Mode => "mode",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::domain::{RegisterValue, SnapshotEntry};

    fn def(name: &str, addr: u16, group: Option<&str>, unit: Option<&str>) -> RegisterDef {
        RegisterDef {
            name: name.into(),
            kind: RegisterKind::Input,
            address: addr,
            value_type: RegisterValueType::U16,
            writable: false,
            unit: unit.map(str::to_owned),
            group: group.map(str::to_owned),
            display_name: None,
        }
    }

    #[test]
    fn format_empty_snapshot_is_empty() {
        assert_eq!(format_snapshot(&Snapshot::new(), &[]), "");
    }

    #[test]
    fn format_groups_entries_by_section() {
        let registers = vec![
            def("a", 0, Some("State"), None),
            def("b", 1, Some("Temperatures"), Some("°C")),
            def("c", 2, Some("State"), None),
        ];
        let snap = Snapshot {
            entries: vec![
                SnapshotEntry {
                    name: "a".into(),
                    value: RegisterValue::U16(1),
                    unit: None,
                },
                SnapshotEntry {
                    name: "b".into(),
                    value: RegisterValue::U16(22),
                    unit: Some("°C".into()),
                },
                SnapshotEntry {
                    name: "c".into(),
                    value: RegisterValue::U16(3),
                    unit: None,
                },
            ],
        };
        let out = format_snapshot(&snap, &registers);
        let expected = "\
State
  a  1
  c  3

Temperatures
  b  22 °C
";
        assert_eq!(out, expected);
    }

    #[test]
    fn format_unknown_registers_fall_into_other() {
        let snap = Snapshot {
            entries: vec![SnapshotEntry {
                name: "orphan".into(),
                value: RegisterValue::U16(7),
                unit: None,
            }],
        };
        let out = format_snapshot(&snap, &[]);
        assert!(out.starts_with("Other\n"));
        assert!(out.contains("orphan"));
    }

    #[test]
    fn format_single_entry_appends_unit() {
        let entry = SnapshotEntry {
            name: "supply".into(),
            value: RegisterValue::Temperature(21.5),
            unit: Some("°C".into()),
        };
        assert_eq!(format_single_entry(&entry), "21.5 °C\n");
    }

    #[test]
    fn format_single_entry_without_unit() {
        let entry = SnapshotEntry {
            name: "fan".into(),
            value: RegisterValue::U16(3),
            unit: None,
        };
        assert_eq!(format_single_entry(&entry), "3\n");
    }

    #[test]
    fn format_register_list_renders_header_and_rows() {
        let registers = vec![
            def("supply", 7, Some("Temperatures"), Some("°C")),
            def("power", 2, None, None),
        ];
        let out = format_register_list(&registers);
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.starts_with("NAME"));
        assert!(header.contains("ADDR"));
        assert!(header.contains("GROUP"));

        let row1 = lines.next().unwrap();
        assert!(row1.contains("supply"));
        assert!(row1.contains("0x07 (7)"));
        assert!(row1.contains("temperature_x10") || row1.contains("u16"));
        assert!(row1.contains("Temperatures"));

        let row2 = lines.next().unwrap();
        assert!(row2.contains("power"));
        assert!(row2.contains("Other"));
    }

    #[test]
    fn format_register_list_empty() {
        assert_eq!(format_register_list(&[]), "");
    }
}
