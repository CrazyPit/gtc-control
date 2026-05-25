# gtc-control

A terminal controller for **GTC Oasis** ventilation units over Modbus
TCP. Ships a small CLI for scripting plus a full-screen interactive
TUI (ratatui + crossterm) with live polling, edit-in-place controls,
and a Settings screen.

The GTC unit's Ethernet module (EM-LAN) exposes Modbus TCP on port
502 alongside the official GTC mobile app, so this client coexists
with the vendor app on the same LAN.

> macOS-only by design. There are no `cfg(target_os = ...)` branches
> in the codebase. If you need Linux or Windows, the operations
> layer is platform-agnostic — the macOS scope is only enforced for
> the build and dependencies (single-binary, single Tokio runtime,
> no platform shims).

---

## Features

- **CLI subcommands** for one-shot poll, read-one, write-one, and
  register-catalogue listing.
- **Interactive TUI** with a live status panel, decoded state words
  (power, mode, phase, errors, recovery hints), and edit-in-place
  controls for power, mode, temperature setpoint, and fan setpoints.
- **Four-option mode picker** (Ventilation / Heating / Cooling /
  Climate) backed by a read-modify-write path on the packed
  `Dev_Keys_2` configuration register — the surrounding 14 bits are
  preserved on every write.
- **Settings screen** (`s` from the main view) for editing the
  Modbus endpoint, poll cadence, and per-row UI visibility toggles
  without leaving the terminal. Saves atomically to
  `~/.gtc-control/config.yml` on close.
- **Single-session Modbus actor** — only one TCP connection is held
  open at any time, which the EM-LAN module strictly requires (it
  locks up under parallel sessions or rapid open/close cycles).

## Install / build

Requirements:

- macOS 12 or later.
- A recent stable Rust toolchain (`rustup install stable`).
- Network reachability to the GTC controller (default Modbus TCP
  port `502`).

```sh
cargo build --release
./target/release/GTC_Control --help
```

On first launch the binary materialises `~/.gtc-control/config.yml`
from the bundled defaults. **Edit `modbus.host`** to point at your
controller — either via the Settings screen (`s`) or by opening the
file directly.

## CLI

```sh
GTC_Control                       # launch the interactive TUI (default)
GTC_Control poll                  # read every register, print snapshot
GTC_Control read <name>           # read one register, print value + unit
GTC_Control set <name> <value>    # write one register
GTC_Control list                  # print the bundled register catalogue
```

Register names mirror the GTC nomenclature so they grep cleanly
against the vendor PDF — `firmware_version`, `state_word_0`,
`supply_air_temp`, `temp_setpoint`, `mode_system`, etc. See
[`docs/Oasis_Registers.md`](docs/Oasis_Registers.md) for the curated
catalogue.

`RUST_LOG` controls log verbosity (default: `gtc_control=info,warn`).
Logs go to stderr; CLI command output goes to stdout.

### Examples

```sh
# Quick health check — should print firmware version like "5.1.0"
GTC_Control read firmware_version

# Set the air-out target to 23.0 °C
GTC_Control set temp_setpoint 23.0

# Pick climate-control (auto heat / cool) mode
GTC_Control set mode_system climate
```

## Interactive TUI

Launched with `GTC_Control` (no subcommand).

| Key             | Action                                          |
|-----------------|-------------------------------------------------|
| `↑` / `↓`       | Navigate between controls                       |
| `Enter`         | Toggle the selected control (Power, Mode) or open the inline editor (Setpoint, Fans) |
| `↑` / `↓` in edit | Step by ±0.5 °C (setpoint) or ±1 (fans)       |
| digits / `.`    | Type a numeric value                            |
| `Enter` in edit | Validate and write                              |
| `Esc` in edit   | Cancel the edit                                 |
| `s`             | Open the Settings screen                        |
| `q` / `Esc` / `Ctrl-C` | Quit                                     |

Mode cycles **Ventilation → Heating → Cooling → Climate → Ventilation**
when pressed on the Mode row. When **Climate** is selected and the
controller is actively heating or cooling, the active state is shown
inline (`Climate  (heating now)`).

## Settings screen

| Key             | Action                                          |
|-----------------|-------------------------------------------------|
| `↑` / `↓`       | Navigate                                        |
| `Space`         | Toggle the selected boolean                     |
| `Enter`         | Edit the selected text/numeric field            |
| `Esc`           | Save and close (atomic write to disk)           |

Sections:

- **Connection** — `host`, `port`, `poll interval`.
- **Temperatures** — show/hide each individual sensor row.
- **Modes** — show/hide Heating / Cooling / Climate options in the
  Mode picker. Ventilation is always selectable.
- **Fans** — show/hide the Exhaust-fan rows (the supply fan is
  always shown).

Connection changes (`host`, `port`, `poll interval`) take effect on
the **next launch** — the Modbus actor binds to the values it was
started with. UI-visibility toggles apply immediately.

## Configuration

Two YAMLs feed the runtime:

- **`config/default.yml`** — bundled at build time via `include_str!`.
  Source of truth for the register catalogue (addresses, value
  types, group labels) and the factory-default connection + UI
  preferences. Users do not edit this.
- **`~/.gtc-control/config.yml`** — user-editable subset (Modbus
  endpoint, poll cadence, UI visibility). Materialised on first
  launch from the bundled defaults; rewritten by the Settings screen
  on close.

Top-level shape of the user file:

```yaml
modbus:
  host: 192.168.169.102
  port: 502
  unit_id: 1
  timeout_ms: 1500

poll:
  interval_seconds: 5

ui:
  temperatures:
    supply_air_temp: true
    return_water_temp: true
    outdoor_temp: true
    room_temp: true
    recuperator_outlet_temp: true
  modes:
    heating: true
    cooling: true
    climate: true
  exhaust_fan: true
```

Comments in the user file are **not** preserved across
Settings-screen saves. Edit either via the screen or by hand, not
both at the same time.

## Project layout

```
src/
  domain.rs    Pure types: RegisterKind, RegisterValueType, RegisterDef,
               RegisterValue (including ModeSelection), Snapshot.
  config.rs    Bundled + user-config loader, validator, atomic writer.
  modbus.rs    ModbusClient trait + tokio-modbus TCP impl + in-memory
               FakeModbusClient for tests.
  app.rs       poll_once, read_one, set_value (with the Dev_Keys_2
               read-modify-write path).
  status.rs    Decoders for State_0 / State_1 / Error_Code /
               Error_Code_1, plus ActiveMode / Priority / OperationPhase.
  tui.rs       Full-screen interactive view (ratatui + crossterm) and
               the Settings screen.
  lib.rs       Module declarations + CLI-side formatting helpers.
  main.rs      Composition root: clap CLI + tokio runtime.
config/default.yml      Bundled configuration (registers + defaults).
docs/                   Hardware reference + Wirenboard integration brief.
```

See [`CLAUDE.md`](CLAUDE.md) for the engineering rules this codebase
follows (lint policy, layering, testing, comment policy).

## Documentation

- [`docs/Oasis_Registers.md`](docs/Oasis_Registers.md) — curated
  register reference distilled from the GTC PDF (addresses, value
  encodings, bit-field tables, EEPROM warnings).
- [`docs/Ethernet_Module_Setup.md`](docs/Ethernet_Module_Setup.md) —
  how to find the EM-LAN's IP via the controller's touch panel.
- [`docs/Wirenboard_Integration.md`](docs/Wirenboard_Integration.md)
  — a self-contained brief for integrating the same controller into
  a [Wirenboard](https://wirenboard.com/) system through
  `wb-mqtt-serial` + `wb-rules`, including the read-modify-write
  pattern needed for the mode register.

## Safety guards

- **Fan setpoint floor of 1.** Writing `0` to `fan_speed_supply` or
  `fan_speed_exhaust` is rejected by both the CLI (`AppError::Refused`)
  and the TUI (range validator). The controller accepts `0` at the
  bus level but with the heater stage on the lack of airflow burns
  the heat exchanger out within seconds. Use `power` to turn the
  unit off.

## Known caveats

- **EM-LAN module: one TCP slot at a time.** The Ethernet module
  locks up under parallel Modbus sessions or rapid connection
  open/close cycles. The TUI uses a single persistent connection
  through a dedicated actor. Do not run a second Modbus client
  against the same controller while the TUI is running.
- **EEPROM-backed registers** (notably `Dev_Keys_2`, used for the
  mode selector) have a finite write-cycle budget. The code dedups
  no-op writes; downstream integrations should do the same.
- **Live writes have been verified for** `power`, `temp_setpoint`,
  `fan_speed_supply`, `fan_speed_exhaust`. The `mode_system`
  read-modify-write path is covered by unit tests against the fake
  Modbus client but had not been end-to-end-verified against the
  reference unit at time of writing — the EM-LAN was in its
  locked-up state during the final verification window.

## License

MIT. See [`LICENSE`](LICENSE).
