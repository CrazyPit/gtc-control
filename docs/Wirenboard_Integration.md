# GTC Oasis ventilation controller — Wirenboard integration brief

A self-contained reference for integrating a GTC Oasis ventilation
unit (with EM-LAN Ethernet module) into a Wirenboard controller via
Modbus TCP, exposed as virtual MQTT devices for the Web UI and
`wb-rules` scripting.

This document distils ~2 weeks of reverse-engineering against a real
unit running firmware **5.1.0**. Everything below has either been
verified on hardware or is taken verbatim from the official GTC PDF
(*Oasis Registers v5*). Where the PDF is wrong or incomplete, the
mismatch is called out explicitly.

---

## Table of contents

1. [Quick reference](#quick-reference)
2. [Hardware and transport](#hardware-and-transport)
3. [EM-LAN connection quirks — read this first](#em-lan-connection-quirks--read-this-first)
4. [Modbus addressing convention](#modbus-addressing-convention)
5. [Value encodings](#value-encodings)
6. [Register catalogue](#register-catalogue)
7. [Bit-field decoders](#bit-field-decoders)
8. [Mode register (Dev_Keys_2) — read-modify-write required](#mode-register-dev_keys_2--read-modify-write-required)
9. [EEPROM-backed registers — write sparingly](#eeprom-backed-registers--write-sparingly)
10. [Fan speed scale](#fan-speed-scale)
11. [Live-verified facts from the reference unit](#live-verified-facts-from-the-reference-unit)
12. [Polling strategy](#polling-strategy)
13. [Wirenboard integration plan](#wirenboard-integration-plan)
14. [Open questions](#open-questions)

---

## Quick reference

| Property | Value |
|----------|-------|
| Protocol | Modbus TCP |
| Default port | `502` |
| Default slave / unit ID | `1` |
| Reference unit IP | `192.168.169.102` (user-configurable on the controller itself) |
| Reference firmware | `5.1.0` (`0x5100` at input wire address `0`) |
| Recommended request timeout | `1500 ms` |
| Recommended poll cadence | `5 s` for the full curated catalogue |
| Concurrent connection limit | **1** — see [EM-LAN quirks](#em-lan-connection-quirks--read-this-first) |
| Coil / discrete-input usage | **None** — controller exposes only input and holding registers |

The controller does **not** expose coils or discrete inputs. Power
on/off lives in a holding register (`Power_Dev`, bit 0). Everything is
either an input register (FC `0x04`, read-only sensors and state) or a
holding register (FC `0x03` read / `0x06` write, setpoints and
configuration).

---

## Hardware and transport

The GTC Oasis is a programmable ventilation controller targeted at
small-to-mid commercial HVAC installations: supply + exhaust fans, a
heater stage (electric / water / combined), an optional cooler stage
(direct expansion / fancoil / chilled water), and an optional heat
recovery exchanger (plate / rotary / glycol). The user-facing panel
("ОАЗИС" touchscreen) and the optional EM-LAN Ethernet module both
talk to the same controller, which is also the Modbus slave.

Transport choices:

- **Modbus RTU** over RS-485 — the native, multi-drop bus the controller
  was originally designed for. Slave addresses and baud rates for three
  RTU ports are configurable in holding registers `0xC5..=0xCC`. Not
  used in this project; mentioned only because the same register
  catalogue applies.
- **Modbus TCP** through the EM-LAN module — what this document
  assumes. The module bridges Modbus TCP requests on port 502 to the
  controller's internal bus.

The proprietary GTC "Remote Access" protocol exists alongside Modbus
(used by the official mobile app and `remoteaccess.gtcontrollers.com`).
**Avoid it.** Modbus TCP is the open, well-documented path. The user
file at `~/.gtc-control/config.yml` on the reference development
machine talks only Modbus TCP and that is the contract this document
extends.

### Discovering the EM-LAN's IP

The IP is assigned by the controller's own touch panel, not by the
module's web UI:

1. Press and hold the panel ~5 seconds → enter Engineer/Service menu.
2. Navigate to **СТАРТ ▸ ПУСК ▸ ОБЩИЕ ▸ ETHERNET**.
3. The panel shows `IP-АДРЕС:` — that is the address to put in the
   Modbus TCP client.

The two other fields (`РЕГИСТР. НОМЕР`, `РЕГИСТР. КЛЮЧ`) are for the
GTC Remote Access service and are irrelevant for Modbus.

The EM-LAN itself has a tiny config web UI on the same IP / port 80;
useful for verifying connectivity but not for Modbus.

---

## EM-LAN connection quirks — read this first

These are not documented anywhere. They were learned the hard way and
will bite anyone who treats the module like a normal Modbus TCP slave.

### Only one TCP session at a time

The EM-LAN module accepts exactly **one** concurrent Modbus TCP slot.
Open a second TCP connection to port 502 while the first is alive and
the module silently stops responding on **both** connections — often
for several minutes, sometimes until power-cycled.

**Implication for Wirenboard:** route everything through a single
`wb-mqtt-serial` instance bound to a single TCP "port" definition. Do
**not** add a second instance for testing, do **not** run `mbpoll` /
`modpoll` / a Python script against the same controller while
`wb-mqtt-serial` is active.

### Rapid open/close locks the module

Even single-threaded use lock the EM-LAN if the client opens a fresh
TCP connection for every request. Successive invocations of the
reference CLI (each spinning up its own connection) reproducibly
froze the module after 3–5 calls in quick succession.

**Implication:** keep one persistent connection open for the lifetime
of the integration. `wb-mqtt-serial` does this by default for TCP
ports; just make sure the device-level "connection lifetime" isn't
set to per-request.

### Recovery

If the module locks up:

1. Stop all clients hitting it.
2. Wait 1–3 minutes. The module usually recovers on its own.
3. If still unresponsive, power-cycle either the controller or just
   the EM-LAN module.

There is no graceful "Modbus exception" coming back — connections
just hang or get refused.

### Recommended client behaviour

- Single persistent TCP session.
- Sequential requests; never pipeline.
- Per-request timeout around 1500 ms.
- On a transport-level failure (timeout, refused, reset): close the
  socket, wait at least 500 ms, reconnect lazily on the next request.
- Don't reconnect aggressively in a tight loop — that's how the module
  freezes in the first place.

---

## Modbus addressing convention

The official PDF lists every register as `0xNN (NN)` starting at
`0x01 (01)`. This is a **1-based register number**. On the wire,
Modbus uses 0-based addresses, so:

```
wire_address = documented_address − 1
```

This is the standard convention `tokio-modbus`, `pymodbus`, `mbpoll`,
and most others use — pass the wire address as the Modbus PDU offset
directly. `wb-mqtt-serial` templates also use 0-based wire addresses.

**Verification step before trusting the register map:** read **input
register at wire address `0`** with count `1`. Expected response:
the firmware version word, e.g. `0x2311` for firmware `2.3.17` or
`0x5100` for `5.1.0`.

If wire `0` returns nothing meaningful but wire `1` does, the
particular client library you're using is 1-based internally — adjust
all addresses uniformly by ±1 and re-verify.

The reference unit returned `0x5100` at wire `0`; the rest of this
document assumes the same library convention.

---

## Value encodings

| Documented form | Bytes | Decode | Example |
|-----------------|-------|--------|---------|
| `unsigned int` | 2 | `u16` | `60` → 60 |
| `signed int` | 2 | `i16` (two's complement reinterpretation) | `0xFFFF` → −1 |
| `16 bit` (raw) | 2 | display as hex | `0x5100` |
| `(°C × 10)` | 2 | `i16`, divide by 10 | `215` → 21.5 °C; `-50` (`0xFFCE`) → −5.0 °C |
| Packed byte pair | 2 | high byte / low byte hold two independent fields | `Time_Min` (low) / `Time_Hour` (high) at `0x04` |
| Bit field | 2 | extract named bits — see decoders below | `State_0 = 449` |

Setpoint ranges (clamp on writes, the controller silently rejects
out-of-range):

| Setpoint | Raw range | Effective |
|----------|-----------|-----------|
| Temperature (`Temp_Target`) | `150..=300` | 15.0–30.0 °C |
| Humidity (`e_Hum_Target`) | `20..=95` | 20–95 % |
| CO₂ (`e_Room_CO2_Target`) | `500..=2000` | 500–2000 ppm |
| Fan speed | `0..=10` | depends on installation, see [Fan speed scale](#fan-speed-scale) |

### Packed byte pairs

A handful of registers pack two unrelated values into the high and
low bytes of a single word:

- Time: `Time_Min` (low) + `Time_Hour` (high) at `0x04`; same scheme
  for `Time_Sec` + `Time_DOW` at `0x05`, day/month at `0x07`, etc.
- Date: `Time_Date` (low) + `Time_Month` (high) at `0x07`.
- Service phone number: 16 registers from `0x40..=0x4F`, two ASCII
  characters per word.
- IP / gateway / MAC: 6 registers from `0x15E..=0x164`.
- Fan-speed timer settings: each daily timer's `Fan_Target_1..2` are
  packed into one word, `_3..4` into the next.

For Wirenboard, the Modbus template system supports extracting
high/low bytes via `format: u8` with `address` and a bit-offset; or
you can ignore the packed registers entirely for v1 (the curated
catalogue below doesn't need any of them).

---

## Register catalogue

This is the curated v1 set — every register the reference integration
actually reads or writes. The full PDF has roughly 80 input registers
and 360 holding registers; the rest are installation-time
configuration and weekly timer schedules that don't need to be in MQTT.

**Wire addresses** below are the documented address minus 1 (see
[Modbus addressing convention](#modbus-addressing-convention)).

### Input registers (read-only, FC `0x04`)

| Wire | Doc | Name | Type | Group | Meaning |
|-----:|-----|------|------|-------|---------|
| `0` | `0x01(01)` | `Firmware_Vers` | `u16` raw hex | Identity | Firmware version word. Use to verify addressing. |
| `2` | `0x03(03)` | `State_0` | `u16` bitfield | State | Device state word 0 — see [State_0 decoder](#state_0-input-wire-2). |
| `3` | `0x04(04)` | `State_1` | `u16` bitfield | State | Device state word 1 — see [State_1 decoder](#state_1-input-wire-3). |
| `4` | `0x05(05)` | `Error_Code` | `u16` bitfield | State | Active error mask — see [Error_Code decoder](#error_code-input-wire-4). |
| `5` | `0x06(06)` | `Error_Code_1` | `u16` bitfield | State | Auxiliary error / informational mask. |
| `7` | `0x08(08)` | `Tkan_x10` | `i16` × 10 °C | Temperatures | Supply-air (T1) temperature. |
| `8` | `0x09(09)` | `Tobr_x10` | `i16` × 10 °C | Temperatures | Return-water (T2) temperature. |
| `11` | `0x0C(12)` | `TNar_x10` | `i16` × 10 °C | Temperatures | Outdoor-air (T3) temperature. |
| `14` | `0x0F(15)` | `ZagrFiltr1` | `u16` (%) | Filters | Filter 1 contamination, 0–100 %. |
| `25` | `0x1A(26)` | `Fan_State_1` | `u16` | Fans | Current supply-fan speed (see [scale](#fan-speed-scale)). |
| `30` | `0x1F(31)` | `Fan_State_2` | `u16` | Fans | Current exhaust-fan speed (firmware 5.0+). |
| `74` | `0x4B(75)` | `TKomn_x10` | `i16` × 10 °C | Temperatures | Room (T4) temperature (firmware 5.0+). |
| `76` | `0x4D(77)` | `Trecp_x10` | `i16` × 10 °C | Temperatures | Recuperator-outlet (T5) temperature (firmware 5.0+). |
| `84` | `0x55(85)` | `ZagrFiltr2` | `u16` (%) | Filters | Filter 2 contamination, 0–100 %. |

### Holding registers (read/write, FC `0x03` read, `0x06` write)

| Wire | Doc | Name | Type | EEPROM? | Meaning |
|-----:|-----|------|------|:-------:|---------|
| `2` | `0x03(03)` | `Power_Dev` | `u16` bit 0 | no (RAM) | Unit on (`1`) / off (`0`). Bits 1–15 reserved. **Safe to write freely.** |
| `31` | `0x20(32)` | `Temp_Target` | `i16` × 10 °C | yes | Air-out setpoint, raw range `150..=300`. |
| `32` | `0x21(33)` | `Fan_Target_1` | `u16` | yes | Supply-fan setpoint. |
| `33` | `0x22(34)` | `Fan_Target_2` | `u16` | yes | Exhaust-fan setpoint (firmware 5.0+). |
| `35` | `0x24(36)` | `e_Hum_Target` | `u16` (%) | yes | Humidity setpoint, `20..=95`. |
| `36` | `0x25(37)` | `e_Room_CO2_Target` | `u16` (ppm) | yes | Room CO₂ setpoint, `500..=2000`. |
| `86` | `0x57(87)` | `Dev_Keys_2` | `u16` packed bits | yes | Operation-mode + auxiliary device options. See [Mode register](#mode-register-dev_keys_2--read-modify-write-required) — **bits 0..1 only**. |

Lots more holdings exist (Type_Dev at `0x01` for installation config,
PID coefficients, weekly timer schedules at `0x70..=0xB5`, Ethernet
module settings at `0x15E..=0x164`, etc.) but they don't need to be
in the day-to-day MQTT surface.

---

## Bit-field decoders

These are the read-only state words the controller publishes. For
each, decode them on the integration layer (e.g. `wb-rules`) into
distinct, named MQTT controls — that's the Web-UI-friendly shape.

### State_0 (input wire `2`)

| Bit | Meaning |
|-----|---------|
| 0 | `power_on` — unit is currently running (`1`) or stopped (`0`) |
| 1 | `transitioning` — moving toward the state encoded in bit 0 |
| 2–5 | reserved |
| 6 | `heating_available` — heating stage is physically installed |
| 7 | `cooling_available` — cooling stage is physically installed |
| 8 | `active_mode_heating` — controller is currently heating (`1`) vs cooling (`0`); only meaningful when both stages are installed |
| 9 | `timer_today` — a "next 24 h" timer entry is active |
| 10 | `timer_week` — a "next 7 days" timer entry is active |
| 11–12 | `priority` — sensor priority: `0`=none / temperature setpoint, `1`=humidity, `2`=CO₂, `3`=pressure |
| 13–15 | reserved |

**Live example:** the reference unit returned `449` =
`0b0000_0001_1100_0001`, which decodes to: power on, both heat and
cool installed, currently heating, no timers, no priority. Consistent
with the mobile app's display.

Note that `active_mode_heating` is meaningful only when both
heat and cool stages are installed (bits 6 and 7 both `1`). With just
heating installed, the controller is always "heating" when it does
anything thermal; with just cooling, always "cooling"; with neither,
the unit is ventilation-only.

### State_1 (input wire `3`)

Bits 0–4 encode the **current operation phase** as a small integer
(not a bitmask). Bits 5–15 are reserved.

| Value | Phase |
|------:|-------|
| `0` | Idle (no transition in progress) |
| `1` | Opening air damper |
| `2` | Pre-heating heater before fan start |
| `3` | Starting fan |
| `4` | "Northern start" (slow warm-up sequence for cold climates) |
| `5` | Fan coasting (winding down to stop) |
| `6` | Closing air damper |
| `7` | Purging electric heater after shut-off |
| `8` | Opening hot-water valve |
| `9` | Closing hot-water valve |
| `10` | Opening cold-water valve |
| `11` | Closing cold-water valve |
| `12` | Accelerating rotor recuperator |

Anything outside this list should be displayed as "Unknown phase (raw)".

### Error_Code (input wire `4`)

| Bit | Meaning |
|-----|---------|
| 0 | T1 channel-temperature sensor — open or short |
| 1 | T2 return-water sensor — open or short |
| 2 | T3 outdoor sensor — open or short |
| 3 | Filter 1 pressure sensor fault |
| 4 | Filter 1 fully clogged (alarm) |
| 5 | No coolant in system |
| 6 | Frost risk: return-water temperature below 5 °C |
| 7 | Frost risk: capillary freeze sensor tripped |
| 8 | Frost risk: channel air temperature below 5 °C |
| 9 | Fan 1 (X1) pressure sensor — open / fault |
| 10 | Fan 1 (X1) fault |
| 11 | **FIRE alarm** |
| 12 | reserved |
| 13 | Heater overheat |
| 14 | Cooler 1 (ККБ1) pressure fault |
| 15 | Cooler 2 (ККБ2) pressure fault |

Zero = no active errors. Multiple bits can be set simultaneously.

### Error_Code_1 (input wire `5`) — auxiliary / notes

The PDF's `Error_Code_1` is a mix of duplicate-channel diagnostics
(open vs short for each sensor) plus informational flags. The latter
are worth surfacing as separate MQTT controls; the former are
debug-only and can be aggregated into a single "aux errors" hex topic.

| Bit | Meaning |
|-----|---------|
| 0–3 | Per-channel "short/open" detail for T1..T3 + filter 1 (debug) |
| 4 | **Note:** system overheat — setpoint not reached even with heat fully off |
| 5 | **Note:** system undercool — setpoint not reached with heat fully on |
| 6 | **Note:** remote-stop input is asserted |
| 7 | **Note:** auto fan-speed reduction is enabled |
| 8–9 | reserved |
| 10 | **Note:** "Northern start" mode is currently active |
| 11 | **Note:** recuperator outlet below 0 °C |
| 12 | **Note:** recuperator above target temperature |
| 13 | **Note:** recuperator icing — preheat is active |
| 14 | **Note:** recuperator de-icing fan-speed reduction is active |
| 15 | **Note:** smooth speed-reduction mode is active |

**Live example:** the reference unit returned `16` =
`0b0001_0000`, bit 4 set, meaning "system overheat — setpoint not
reached even with heat off". Consistent with the outdoor air being
warmer than the temperature setpoint at the time of capture.

### Type_Dev (holding `0x01`) — installation configuration (read-only-in-practice)

This is the static installation profile. The controller reads it
once at boot. Don't surface this register in the Web UI as a control
— it describes hardware capabilities, not user choices.

| Bits | Field | Meaning |
|------|-------|---------|
| 0–3 | Heater | `0`=none, `1`=electric, `2`=water, `3`=combined |
| 4–7 | Cooler | `0`=none, `1`=DX (ККБ), `2`=fancoil, `3`=chilled water, `4`=inverter DX, `5`=combined, `6`=dual DX serial, `7`=dual DX serial w/ rotation, `8`=dual DX rotation-only |
| 8–11 | Recuperator | `0`=none, `1`=plate, `2`=rotary, `3`=glycol, `4`=refrigerant |
| 12–15 | reserved | |

Reading this once at integration boot is fine. Writing it is **not**
fine — it's EEPROM-backed and changes physical-hardware assumptions
the controller relies on for safety interlocks.

---

## Mode register (Dev_Keys_2) — read-modify-write required

This is the single most error-prone register in the catalogue. Read
this section carefully before writing any code that touches `0x57`.

### Layout

`Dev_Keys_2` (holding wire address `86`, documented `0x57(87)`) is a
**packed configuration word**. The mobile app's "Воздух" dropdown
(which exposes the four operation modes) lives in **bits 0..1** —
just two bits. **The remaining 14 bits carry unrelated installation
configuration** that the user has no business touching from the Web
UI but which **must be preserved by any write**.

| Bits | Field | Values |
|------|-------|--------|
| 0–1 | **System operation mode** | `0`=ventilation only, `1`=heating only, `2`=cooling only, `3`=climate control (auto) |
| 2 | Timer enabled | `0`=no, `1`=yes |
| 3–4 | Humidifier type | `0`=none, `1`=duct sensor, `2`=room + duct sensors, `3`=humidifier priority |
| 5–6 | CO₂ sensor type (firmware ≤ 5.0) | `0`=none, `1`=NO contact, `2`=NC contact, `3`=analog |
| 7–8 | Outdoor temperature sensor type (firmware ≤ 5.0) | same encoding as bits 5–6 |
| 9 | Water heater pump control (firmware 3.0.0–3.1.0) | `0`=off, `1`=on |
| 10 | Water heater pump password (firmware 5.0.5+) | `0`/`1` |
| 11 | Speed reduction style | `0`=fast, `1`=smooth |
| 12 | Indoor humidity sensor location (firmware 3.0.18+) | `0`=internal, `1`=external |
| 13–14 | Water heater pump mode (firmware 3.1.1+) | `0`=none, `1`=manual, `2`=automatic |
| 15 | Water-heater warm-up mode | `0`=standard, `1`=heat exchanger station |

### Mode bits 0..1 — the firmware encoding vs the mobile app

The mobile app's four-option "Воздух" dropdown maps to the bits as:

| App label | App position | Register value |
|-----------|--------------|----------------|
| Вентиляция (Ventilation) | 1st | `0` |
| Нагрев (Heating) | 2nd | `1` |
| Охлаждение (Cooling) | 3rd | `2` |
| Климат-контроль (Climate / auto) | 4th | `3` |

The official PDF documents values `1`, `2`, `3` only ("with version
1.0.21" added the latter two). Value `0` is undocumented in the PDF
but is what the mobile app writes when the user picks "Вентиляция" —
ventilation-only mode, both heat and cool stages stay off regardless
of setpoint. Treat `0` as a first-class value, not an error state.

### Read-modify-write — non-negotiable

Writing a raw mode value (`0`–`3`) directly to `Dev_Keys_2` will trash
bits 2..15. Symptoms range from "humidifier suddenly disabled" to
"water heater pump no longer auto-starts on cold mornings", and the
user has no obvious way to see what changed.

The correct write path:

```
1. Read holding register 86, count = 1 → current_word
2. cleared = current_word & 0xFFFC          (mask out bits 0..1)
3. new_word = cleared | (new_mode_bits & 0x0003)
4. Write holding register 86 = new_word
```

Under a single-Modbus-session architecture (which is mandatory for
the EM-LAN anyway) the read-modify-write window is safe — no other
client can race a partial write.

### EEPROM caveat

`Dev_Keys_2` is **EEPROM-backed**. Don't write it in a polling loop or
on every Web-UI control update without dedup. The cell rating is
~100k–1M writes; at one write per second you'd hit the lower bound in
~28 hours of accidentally-spammy rules.

Practical rule: write only when the resolved value would actually
change. Compare against the most recent read, skip the bus
transaction if equal.

---

## EEPROM-backed registers — write sparingly

The PDF flags most holding registers as `(EEPROM)`. Cells have a
finite write-cycle budget; spamming them in a polling loop is how
controllers die.

**Safe to write frequently (RAM-only, refreshes immediately):**

- `Power_Dev` (`0x03`, wire `2`)
- `Temp_Target` (`0x20`, wire `31`)
- `Fan_Target_1` (`0x21`, wire `32`)
- `Fan_Target_2` (`0x22`, wire `33`)
- `e_Hum_Target` (`0x24`, wire `35`)
- `e_Room_CO2_Target` (`0x25`, wire `36`)

**EEPROM-backed — write only on explicit user action, dedup against last-read value:**

- `Type_Dev` (`0x01`, wire `0`) — installation profile
- `Dev_Keys_0..3` (`0x55..=0x58`, wires `84..=87`) — installation
  options including the mode register
- Weekly timer schedule (`0x70..=0xB5`, wires `111..=180`)
- PID coefficients (`0xD2..=0xDF`, `0xE1..=0xE3`, `0xF0..=0xF3`,
  `0xF5..=0xF8`)
- Sensor calibration offsets (`0x50..=0x54`)
- Ethernet module config (`0x15E..=0x164`)

### Dedup pattern for Wirenboard rules

```javascript
// pseudocode for wb-rules
defineRule("mode_write", {
  whenChanged: "gtc_ventilation/controls/mode/on",
  then: function(newValue, devName, ctlName) {
    var current = dev["gtc_ventilation"]["mode_raw_internal"];
    var target_bits = encodeModeBits(newValue);
    var cleared = current & 0xFFFC;
    var next = cleared | target_bits;
    if (next === current) return;          // no-op; protect EEPROM
    writeRegister(86, next);
  }
});
```

`mode_raw_internal` here is a "shadow" control that mirrors the last
known raw `Dev_Keys_2` word, updated by the poll loop. Same pattern
applies to anything else where the user-visible MQTT control is a
fragment of a packed register.

---

## Fan speed scale

The numeric range for `Fan_State_*` (input) and `Fan_Target_*`
(holding) depends on how the fan is physically wired. The relevant
configuration is in **`Fan_Type_1` (holding `0x11`, wire `16`)** and
**`Fan_Type_2` (holding `0x12`, wire `17`)** — both EEPROM-backed,
both set at installation time. The high byte of each holds the mode:

| High byte | Mode | Speed range |
|-----------|------|-------------|
| `0` | Discrete (multi-tap) | `0` (off), `1..=3` |
| `1` | Binary (3-bit) | `0` (off), `1..=7` |
| `2` | Analog 0–10 V | `0` (off), `1..=10` |

The low byte holds the count of speed levels.

For the Web UI control: read `Fan_Type_1` once at startup, derive the
max value, and expose `supply_fan_setpoint` as `range` with that
max. Same for `Fan_Type_2` / `exhaust_fan_setpoint`. If you can't
read `Fan_Type_*` reliably, default to `0..=10` and let the controller
silently clamp out-of-range values (it ignores them).

---

## Live-verified facts from the reference unit

For sanity-checking against the documentation. Reference unit was a
single GTC Oasis with EM-LAN, firmware **5.1.0**, both heating and
cooling stages installed, no humidifier, no CO₂ sensor.

| What was read | Wire address | Raw value | Decoded |
|---------------|------:|-----------|---------|
| Firmware version | input `0` | `0x5100` | `5.1.0` |
| `State_0` | input `2` | `449` (`0x01C1`) | power on; heat & cool installed; currently heating; no priority; no timers |
| `State_1` | input `3` | `0` | Idle phase |
| `Error_Code` | input `4` | `0` | no active errors |
| `Error_Code_1` | input `5` | `16` (`0x0010`) | bit 4 — system overheat note |
| `Tkan_x10` (supply air) | input `7` | (varies) | matched panel reading |
| `Temp_Target` | holding `31` | `225` | 22.5 °C (matched mobile app) |
| `Power_Dev` | holding `2` | `1` | on |

Verified write round-trips against the live unit:

- `fan_speed_supply` (Fan_Target_1, wire 32): wrote `5`, then `6`, read back each value successfully
- `temp_setpoint` (Temp_Target, wire 31): wrote `22.5 → 23.0 → 22.5` (raw `225 → 230 → 225`), read back each value successfully
- `power` (Power_Dev, wire 2): toggled and confirmed via state_word_0 bit 0

The `Dev_Keys_2` write path is implemented in the reference client
but was **not yet** confirmed end-to-end on the live unit at time of
writing — the EM-LAN was in its locked-up state during the final
verification window. The read-modify-write logic itself is unit-tested.

---

## Polling strategy

Recommended polling for an interactive Web UI:

- **Cadence:** every 5 seconds is comfortable and matches the mobile
  app's perceived freshness.
- **Strategy:** read the curated input registers in one sweep, plus
  the writable holdings whose state you want mirrored back into the
  UI (`Power_Dev`, `Temp_Target`, `Fan_Target_*`, `Dev_Keys_2`).
- **Batch reads:** the controller supports multi-register reads, but
  the register map is sparse — reading wire `0..=15` in one call
  returns garbage for the holes (`1`, `6`, `9`, `10`, `12`, `13`).
  Treat them as "don't care" rather than reading every wire address
  individually.
- **Backoff on error:** on a Modbus exception or transport failure,
  drop to a slower retry cadence (e.g. one retry every 10 s) instead
  of hammering the bus.

Don't poll the EEPROM-backed registers more often than once per minute
unless they're known to be RAM-mirrored (`Power_Dev`, the four
`*_Target` registers, `Dev_Keys_2` mode bits) — Reads from EEPROM are
fine, the warning is only about writes. But there's no benefit to
re-reading installation config every 5 seconds either.

---

## Wirenboard integration plan

What follows is a sketch the integrating agent should turn into a
concrete `wb-mqtt-serial` device template + `wb-rules` script(s).

### Single Modbus TCP port in `wb-mqtt-serial`

```yaml
# /etc/wb-mqtt-serial.conf.d/gtc.conf (excerpt — pseudocode)

ports:
  - path: tcp://192.168.169.102:502
    response_timeout_ms: 1500
    poll_interval: 100
    devices:
      - device_type: gtc_oasis
        slave_id: 1
        # Single persistent connection per the EM-LAN quirk —
        # do not enable connection-per-request modes.
```

Where `gtc_oasis` is a custom template (`/usr/share/wb-mqtt-serial/templates/config-gtc-oasis.json`). The template defines channels for each register.

### Suggested MQTT topology

One virtual device, `gtc_ventilation`, with the following controls.
Names are MQTT-friendly snake_case; display names should mirror the
mobile app where reasonable.

#### Read-only sensors

| Control | Type | Source | Notes |
|---------|------|--------|-------|
| `firmware_version` | `text` | input wire `0` | format as `M.m.p` from hex word |
| `supply_air_temp` | `temperature` | input wire `7` | scale ×0.1 |
| `return_water_temp` | `temperature` | input wire `8` | scale ×0.1 |
| `outdoor_temp` | `temperature` | input wire `11` | scale ×0.1 |
| `room_temp` | `temperature` | input wire `74` | scale ×0.1 |
| `recuperator_outlet_temp` | `temperature` | input wire `76` | scale ×0.1 |
| `supply_fan_current` | `value` | input wire `25` | range 0..=10 typically |
| `exhaust_fan_current` | `value` | input wire `30` | range 0..=10 typically |
| `filter1_contamination` | `value` (`%`) | input wire `14` | |
| `filter2_contamination` | `value` (`%`) | input wire `84` | |

#### Read-only decoded state (derived in `wb-rules` from State_0 / State_1 / Error_Code / Error_Code_1)

| Control | Type | Source bits | Notes |
|---------|------|-------------|-------|
| `power_status` | `switch` (read-only) | State_0 bit 0 | mirror of writable `power` |
| `transitioning` | `switch` (read-only) | State_0 bit 1 | "starting" / "stopping" |
| `heating_available` | `switch` (read-only) | State_0 bit 6 | |
| `cooling_available` | `switch` (read-only) | State_0 bit 7 | |
| `active_mode_heating` | `switch` (read-only) | State_0 bit 8 | when in climate-control mode, tells you whether unit is currently heating or cooling |
| `timer_today` | `switch` (read-only) | State_0 bit 9 | |
| `timer_week` | `switch` (read-only) | State_0 bit 10 | |
| `priority` | `text` (read-only) | State_0 bits 11..12 | "none" / "humidity" / "co2" / "pressure" |
| `phase` | `text` (read-only) | State_1 bits 0..4 | "idle" / "opening_damper" / … / "rotor_accelerating" / "unknown(N)" |
| `errors` | `text` (read-only) | Error_Code | comma-separated list of active error labels; empty when zero |
| `notes` | `text` (read-only) | Error_Code_1 | comma-separated list of informational labels |

Wirenboard's `wb-mqtt-serial` template language supports `format: bit` channels that extract a single bit from a register read, so the simple booleans (`power_status`, `transitioning`, the `_available` flags, timers) can be expressed directly in the template without `wb-rules`. The text-valued ones (`priority`, `phase`, `errors`, `notes`) need `wb-rules` to map register reads → enum string.

#### Writable controls

| Control | Type | Register | Encoding |
|---------|------|----------|----------|
| `power` | `switch` | holding wire `2` | write `0` or `1`; bits 1..15 stay zero |
| `temperature_setpoint` | `range` 15.0–30.0 °C, step `0.5` | holding wire `31` | write `round(value × 10)` as `i16`; clamp to `150..=300` |
| `supply_fan_setpoint` | `range` 0–10 | holding wire `32` | direct integer write; range depends on `Fan_Type_1` (see [Fan speed scale](#fan-speed-scale)) |
| `exhaust_fan_setpoint` | `range` 0–10 | holding wire `33` | direct integer write |
| `mode` | `enum` `[ventilation, heating, cooling, climate]` | holding wire `86` | **read-modify-write** on bits 0..1, see below |

#### The `mode` control — `wb-rules` handler

The `mode` control needs RMW logic because writing a raw value would
trash bits 2..15 of `Dev_Keys_2`. Two clean options:

**Option A — Hide the raw register, do RMW in `wb-rules`:**

1. The `wb-mqtt-serial` template reads holding `86` periodically and
   exposes it as an internal numeric topic `_dev_keys_2_raw`
   (read-only, hidden from the Web UI via metadata or simply
   excluded from the default view).
2. A `wb-rules` script subscribes to the user-facing `mode` enum
   topic. On change, it:
   - Reads `_dev_keys_2_raw`.
   - Masks out bits 0..1.
   - ORs in the encoded mode value (`0..=3`).
   - Compares to the current word — if equal, no-op (EEPROM protection).
   - Otherwise writes the merged value back to holding `86`.
3. A second `wb-rules` script subscribes to `_dev_keys_2_raw` and
   updates the user-facing `mode` enum from bits 0..1 on every poll.

This keeps the Web UI clean and the EEPROM safe.

**Option B — Native template-level bit channel (if `wb-mqtt-serial` supports a 2-bit read with read-mask-write):**

Recent `wb-mqtt-serial` versions support `format: u16` channels with
`bit_offset` and `bit_width`. When `bit_width` < 16 and the channel
is writable, the daemon is supposed to do read-modify-write
internally. If your version supports this, define:

```json
{
  "name": "mode",
  "reg_type": "holding",
  "address": 86,
  "format": "u16",
  "bit_offset": 0,
  "bit_width": 2,
  "readonly": false,
  "enum": {
    "0": "ventilation",
    "1": "heating",
    "2": "cooling",
    "3": "climate"
  }
}
```

Verify by writing through it and confirming bits 2..15 are preserved
in a side-by-side full-word read. If the daemon doesn't actually
RMW (i.e. it writes a raw word with zeros above bit 1), fall back to
Option A. The reference client implements Option A in its TUI; the
test suite includes a `preserves_surrounding_bits` assertion against
a `FakeModbusClient`.

#### Sample wb-rules: decoding State_0 bits to text controls

```javascript
defineRule("decode_state_0", {
  whenChanged: "gtc_ventilation/state_0_raw",
  then: function (newValue) {
    var w = newValue | 0;
    var priorityCode = (w >> 11) & 0x03;
    var priority = ["none", "humidity", "co2", "pressure"][priorityCode];
    dev["gtc_ventilation"]["priority"] = priority;
    dev["gtc_ventilation"]["transitioning"] = !!(w & (1 << 1));
    // bit-channels in the template handle the rest.
  }
});
```

#### Sample wb-rules: phase decoder

```javascript
var PHASES = [
  "idle", "opening_damper", "heater_preheating", "fan_starting",
  "northern_start", "fan_coasting", "closing_damper",
  "electric_heater_purge", "opening_hot_water_valve",
  "closing_hot_water_valve", "opening_cold_water_valve",
  "closing_cold_water_valve", "rotor_accelerating"
];

defineRule("decode_state_1", {
  whenChanged: "gtc_ventilation/state_1_raw",
  then: function (newValue) {
    var phase = (newValue | 0) & 0x1F;
    dev["gtc_ventilation"]["phase"] =
      phase < PHASES.length ? PHASES[phase] : ("unknown(" + phase + ")");
  }
});
```

### What NOT to do

- **Don't expose** `Type_Dev`, `Dev_Keys_0..3` (except mode), PID
  coefficients, sensor calibration, or the timer schedule as
  Web-UI controls in v1. They're EEPROM-backed installation knobs;
  surfacing them invites accidental writes that change physical
  hardware assumptions.
- **Don't open a second Modbus session** for monitoring / debugging
  while the integration is running. Use `mosquitto_sub` against the
  MQTT topics instead.
- **Don't tight-loop on connection failures.** Backoff at least
  500 ms between reconnect attempts.
- **Don't write `Power_Dev` as the full word `0x0001`** if you're
  preserving any other bits — bits 1..15 are documented as reserved
  and should stay zero, but if a future firmware uses them, the
  symmetrical read-modify-write applies. For now writing `0` or `1`
  is fine.
- **Don't trust the PDF's `Bit 1...Bit 0` notation** in `Dev_Keys_2`
  as the only legal values — value `0` (ventilation only) works on
  firmware 5.1.0 even though the PDF lists only `1`/`2`/`3`.

---

## Open questions

Things the reference implementation didn't fully verify and that the
Wirenboard integrator may want to confirm on their own unit:

1. **`Dev_Keys_2` value `0` (ventilation only)** — confirmed via the
   mobile app behaviour but not via a live raw-register read against
   the reference unit (EM-LAN was locked at the time). Worth reading
   the register on first integration to confirm the bit pattern when
   "Вентиляция" is selected in the mobile app.
2. **Fan range** — the reference unit's `Fan_Type_1` byte was not
   read; the curated client treats fan speed as `0..=10` and lets
   the controller clamp. Reading `Fan_Type_1` once and clamping at
   the integration layer is the cleaner path.
3. **Multi-register reads** across sparse address ranges — the
   reference client reads each register individually, which is fine
   for ~17 registers but suboptimal. Whether the controller returns
   sane values for "hole" addresses in a batch read is untested.
4. **`Power_Dev` bits 1..15** — documented as reserved, never seen
   non-zero. Safe to write as `0` or `1`. If a future firmware uses
   them, switch to RMW.
5. **`Dev_Keys_2` bits 2..15 typical values on a stock unit** —
   varies with installation. Reading the register once before any
   Web-UI-driven write gives the integrator a baseline.
6. **Modbus exception codes** — the controller's behaviour for an
   illegal-address read or out-of-range write was not exhaustively
   characterised. Treat any exception as transient + log + back off.

---

## References

- Original GTC PDF: *Oasis Registers v5* (Russian, ~30 pages,
  documents all ~440 registers). The structured `Адрес HEX (DEC)`
  table is the source of truth for any register not in the curated
  set above; the bit-field text below each register name is what
  this document distils.
- `docs/Oasis_Registers.md` in the reference client repository —
  a condensed English transcription of the PDF for the curated
  register set.
- `docs/Ethernet_Module_Setup.md` — how to find the EM-LAN IP via
  the touch panel.
- Reference client source at the project root — Rust implementation
  of the read/write logic, including the `Dev_Keys_2` RMW path,
  packaged as both a CLI (`GTC_Control poll/read/set/list`) and a
  TUI. The Modbus layer (`src/modbus.rs`) and the operations layer
  (`src/app.rs`) are the cleanest expression of the contract this
  document describes.
