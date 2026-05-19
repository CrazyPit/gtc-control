# GTC Oasis 5.0 — Modbus register reference

Extracted from `Oasis Registers v5.pdf` (©GTC). This is a curated
working reference; the PDF remains the authoritative source.

## Transport

- **Modbus TCP** on the Ethernet module (EM-LAN), default port `502`.
- **Slave / unit ID:** `1` (default; configurable via holding `0xC5`).
- **No coils, no discrete inputs are documented.** The controller
  exposes only:
  - Input registers (FC 0x04) — read-only state, sensors, current values.
  - Holding registers (FC 0x03 read, FC 0x06 write) — configuration,
    setpoints, EEPROM-backed parameters.

## Addressing — **verify on first contact**

The PDF lists every register as `0xNN (NN)` starting from `0x01 (01)`.
This is almost certainly a **1-based register number**, meaning the
on-the-wire Modbus address is `documented_address − 1`. Standard
practice for `tokio-modbus` and most Modbus libraries is to pass the
zero-based wire address directly.

**Decision:** in `config/default.yml`, write the **wire address**
(documented − 1). The library will send it as-is.

**Validation step before trusting the map:** read input register at
wire address `0` with count `1`. Expected response: the firmware
version word, e.g. `0x2311` for firmware `2.3.17`
(documented as `0x01 (01)` — Firmware_Vers, "Example: 0x2311").

If wire address `0` returns nothing meaningful but wire address `1`
returns the firmware version, the manual is 0-based and all addresses
in `default.yml` must be shifted by +1. Update both this note and the
config in lockstep.

## Value encoding

| Document form        | Wire bytes | Decode                                                 |
|----------------------|------------|--------------------------------------------------------|
| `unsigned int`       | 2 bytes    | `u16`                                                  |
| `signed int`         | 2 bytes    | `i16` (two's complement)                               |
| `16 bit`             | 2 bytes    | Raw hex, e.g. `0x2311` = firmware `2.3.17`             |
| `(°C × 10)`          | 2 bytes    | `i16`, divide by 10. `215` → `21.5 °C`                 |
| Packed byte pair     | 2 bytes    | High byte / low byte hold two independent values       |

Packed pairs appear in time fields (`Time_Min` low byte / `Time_Hour`
high byte at register `0x04`), in fan-speed pairs
(`Fan1_Speed_1` / `_2` packed into a single register), in service-phone
characters (ASCII codes packed two-per-register), and in the IP / MAC
fields at `0x15E`–`0x164`. **The current code base does not parse
packed pairs — we treat the register as raw `u16` and rely on the user
to know which half is which.** A future enhancement is to add a
`pair_u8` value type that decodes both halves; not needed for v1.

Setpoint range: `150..=300` for temperature setpoints (15.0–30.0 °C).

## EEPROM warning

Many holding registers are flagged `(EEPROM)` in the PDF — `Type_Dev`
(0x01), all timer-schedule registers (0x70+), configuration keys
(0x55–0x58), PID coefficients, etc. EEPROM cells have a write-cycle
limit (typically 100k–1M writes). **Do not write to EEPROM-backed
registers in a polling loop.** `GTC_Control` should only write
EEPROM-backed registers in response to an explicit user `set` command,
never automatically. The everyday "on/off, change temperature, change
fan speed" registers (`Power_Dev`, `Temp_Target`, `Fan_Target_*`)
appear to be RAM-only and safe to write frequently.

## Curated v1 register set

These are the registers `GTC_Control` v1 needs. The wire addresses
below are the documented address minus 1 (see the validation note
above). All names mirror the GTC nomenclature so users can grep the
PDF directly.

### Read (Input registers, FC 0x04)

| Wire | Doc       | Name             | Type           | Meaning                                          |
|------|-----------|------------------|----------------|--------------------------------------------------|
| `0`  | `0x01(01)`| Firmware_Vers    | `u16` raw hex  | Firmware version word. Use to verify addressing. |
| `2`  | `0x03(03)`| State_0          | `u16` bitfield | Device state word 0 — see *State_0 bits* below.  |
| `3`  | `0x04(04)`| State_1          | `u16` bitfield | Device state word 1 — see *State_1 bits* below.  |
| `4`  | `0x05(05)`| Error_Code       | `u16` bitfield | Active error mask — see *Error_Code bits*.       |
| `5`  | `0x06(06)`| Error_Code_1     | `u16` bitfield | Auxiliary error/warning mask.                    |
| `7`  | `0x08(08)`| Tkan_x10         | `i16` × 10     | Supply-air (T1) temperature.                     |
| `8`  | `0x09(09)`| Tobr_x10         | `i16` × 10     | Return-water (T2) temperature.                   |
| `11` | `0x0C(12)`| TNar_x10         | `i16` × 10     | Outdoor-air (T3) temperature.                    |
| `14` | `0x0F(15)`| ZagrFiltr1       | `i16` (%)      | Filter 1 contamination, 0–100 %.                 |
| `25` | `0x1A(26)`| Fan_State_1      | `u16`          | Current supply-fan speed (see fan scale below).  |
| `30` | `0x1F(31)`| Fan_State_2      | `u16`          | Current exhaust-fan speed (5.0+ firmware only).  |
| `74` | `0x4B(75)`| TKomn_x10        | `i16` × 10     | Room (T4) temperature (5.0+ firmware only).      |
| `76` | `0x4D(77)`| Trecp_x10        | `i16` × 10     | Recuperator-outlet (T5) temperature (5.0+).      |
| `84` | `0x55(85)`| ZagrFiltr2       | `u16` (%)      | Filter 2 contamination, 0–100 %.                 |

### Write (Holding registers, FC 0x03 / 0x06)

| Wire | Doc       | Name             | Type           | Meaning                                          |
|------|-----------|------------------|----------------|--------------------------------------------------|
| `2`  | `0x03(03)`| Power_Dev        | `u16` bit 0    | `1` = unit on, `0` = unit off. Bits 1–15 reserved.|
| `31` | `0x20(32)`| Temp_Target      | `i16` × 10     | Air-out setpoint, range `150..=300` (15–30 °C).  |
| `32` | `0x21(33)`| Fan_Target_1     | `u16`          | Supply-fan setpoint (see fan scale below).       |
| `33` | `0x22(34)`| Fan_Target_2     | `u16`          | Exhaust-fan setpoint (5.0+ firmware).            |
| `35` | `0x24(36)`| e_Hum_Target     | `u16` (%)      | Humidity setpoint, `20..=95`.                    |
| `36` | `0x25(37)`| e_Room_CO2_Target| `u16` (ppm)    | Room-CO₂ setpoint, `500..=2000`.                 |

### Fan-speed scale

The value range for `Fan_State_*` and `Fan_Target_*` depends on how the
fan is wired (configured at holding `0x11` / `0x12` — `Fan_Type_*`):

| Mode (Fan_Type byte high) | Range        |
|---------------------------|--------------|
| `0` — discrete            | `0`, `1..=3` |
| `1` — binary              | `0`, `1..=7` |
| `2` — analog 0–10 V       | `0`, `1..=10`|

`0` always means *off*. The user's installation determines which scale
applies; the CLI accepts the raw integer and the user is expected to
match it to their unit.

## Bit-field decoders

These are read-only state words. `GTC_Control` v1 only needs to display
them; future versions may surface them as named booleans.

### State_0 (input `0x03`)

| Bit  | Meaning                                                     |
|------|-------------------------------------------------------------|
| 0    | Unit running (1) / stopped (0)                              |
| 1    | Transitioning toward the state in bit 0                     |
| 2–5  | reserved                                                    |
| 6    | Heating stage installed (1) / not (0)                       |
| 7    | Cooling stage installed (1) / not (0)                       |
| 8    | Currently heating (1) / currently cooling (0)               |
| 9    | "Next 24 h" timer entry active (1) / not (0)                |
| 10   | "Next 7 days" timer entry active (1) / not (0)              |
| 11–12| Priority: 0 = none, 1 = humidity, 2 = CO₂, 3 = pressure     |
| 13–15| reserved                                                    |

### State_1 (input `0x04`)

Bits 0–4 hold the current operation phase as a small integer (not a
bitmask):

| Value | Phase                                |
|-------|--------------------------------------|
| 1     | Opening air damper                   |
| 2     | Pre-heating heater before fan start  |
| 3     | Starting fan                         |
| 4     | "Northern start" (cold-climate warm-up sequence) |
| 5     | Fan coasting (winding down)          |
| 6     | Closing air damper                   |
| 7     | Purging electric heater              |
| 8     | Opening hot-water valve              |
| 9     | Closing hot-water valve              |
| 10    | Opening cold-water valve             |
| 11    | Closing cold-water valve             |
| 12    | Accelerating rotary recuperator      |

Bits 5–15: reserved.

### Error_Code (input `0x05`)

| Bit | Meaning                                                              |
|-----|----------------------------------------------------------------------|
| 0   | T1 channel sensor — open or short                                    |
| 1   | T2 return-water sensor — open or short                               |
| 2   | T3 outdoor sensor — open or short                                    |
| 3   | Filter 1 (D5) pressure sensor — open or short                        |
| 4   | Filter 1 100 % clogged (alarm)                                       |
| 5   | No coolant in the system                                             |
| 6   | Frost risk: heater return-water below 5 °C                           |
| 7   | Frost risk: capillary freeze sensor tripped                          |
| 8   | Frost risk: channel air below 5 °C                                   |
| 9   | Fan 1 (X1) pressure sensor — open                                    |
| 10  | Fan 1 (X1) fault                                                     |
| 11  | **FIRE alarm**                                                       |
| 12  | reserved                                                             |
| 13  | Heater overheat                                                      |
| 14  | DX cooler 1 (ККБ1) pressure too low / too high                       |
| 15  | DX cooler 2 (ККБ2) pressure too low / too high                       |

`Error_Code_1` (input `0x06`) holds further breakdowns (open vs short
for each sensor channel, recuperator frost, "smooth speed reduction"
mode flag, etc). Full table is in the PDF — not transcribed here
because v1 does not act on individual sub-bits.

### Type_Dev (holding `0x01`) — installation configuration

Read this once at startup to know what the unit physically contains.
Most installation-specific behaviour keys off this word.

| Bits  | Field      | Meaning (0 = none, otherwise type)                        |
|-------|------------|-----------------------------------------------------------|
| 0–3   | Heater     | 1 = electric, 2 = water, 3 = combined                     |
| 4–7   | Cooler     | 1 = DX (ККБ), 2 = fancoil, 3 = chilled water, 4 = inverter DX, … |
| 8–11  | Recuperator| 1 = plate, 2 = rotary, 3 = glycol, 4 = refrigerant        |
| 12–15 | reserved   |                                                           |

Full mapping is on page 6 of the PDF.

## Full address-range summary

Not every register is transcribed above. The PDF has roughly:

- **Input registers** — `0x01..=0x55` (1..=85 documented, sparse).
  Beyond the curated set: ADC raw codes (`*_Code_*`), CO₂ / humidity
  voltages in mV, fan RPMs, recuperator status sub-bits in
  `Error_Code_2` (`0x46`) and `Error_Code_3` (`0x47`).
- **Holding registers** — `0x01..=0x169` (1..=361 documented, sparse,
  large EEPROM ranges).
  - `0x10..=0x1A` — installation type configuration (heater/cooler/fan).
  - `0x1A..=0x29` — runtime setpoints and panel display options.
  - `0x40..=0x4F` — service phone number (ASCII pairs).
  - `0x50..=0x54` — sensor calibration offsets.
  - `0x55..=0x58` — `Dev_Keys_0..3` — bit fields covering recuperator
    de-icing strategy, cooler-temp-sensor channel, hum/CO₂ presence,
    pump control flags, etc.
  - `0x70..=0xB5` — weekly timer schedule (Sunday → Saturday, 4 timer
    slots per day, each with hour/minute + temp setpoint + fan setpoint).
  - `0xC5..=0xCC` — Modbus RTU slave addresses & baud rates for ports
    1–3 (relevant if reverting to RS-485 instead of TCP).
  - `0xD2..=0xDF` — recuperator PID.
  - `0xE1..=0xE3` — humidifier PID.
  - `0xE6..=0xEE` — sensor type & calibration for T3 / T4 / T5 / humidity.
  - `0xF0..=0xF3` — air-conditioner PID.
  - `0xF5..=0xF8` — water-heater PID.
  - `0x100..=0x113` — discrete input / output / triak / PWM channel
    role assignment (which physical wire does what).
  - `0x114..=0x158` — analog filter calibration curves.
  - `0x15E..=0x169` — Ethernet module config (IP, gateway, MAC,
    remote-server registration).

For any register not in the curated set above, look it up directly in
the PDF — it lists hex address, name, type, value range, and which
firmware version introduced the register.

## What we are *not* doing in v1

- Decoding bit fields into named booleans (errors stay as raw `u16`).
- Decoding byte-packed pairs (time, fan-pair settings, phone-number
  ASCII, IP bytes). The user can still read them as `u16` and interpret
  manually.
- Writing to EEPROM-backed configuration registers. The CLI accepts
  `set` for them as a contract, but v1 documentation recommends against
  using it for anything outside the curated write set above.
- Implementing the proprietary GTC Remote Access protocol. We
  intentionally avoid it — Modbus TCP is the open, vendor-neutral path.
