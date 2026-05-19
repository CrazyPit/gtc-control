# GTC Ethernet Module — relevant notes

Extracted from `Ethernet Module Setup Guide.pdf` (©GTC, January 2020).
Most of that document covers physically mounting the EM-LAN board and
registering the controller against `remoteaccess.gtcontrollers.com` for
the official **GTC Remote Access** mobile app. For our project (Modbus
TCP control from Rust), only two things from this PDF actually matter:

## 1. How to find the controller's LAN IP

From the **OAZIS 5.0** touch panel:

1. ⚙️ → **Настройки пользователя**
2. **5. СЕРВИС**
3. **4. ETHERNET** (centre of screen shows the word "Вывод")

The screen then shows:

- `IP-АДРЕС:` — the local IPv4 address assigned to the EM-LAN module.
  This is the address baked into `config/default.yml` as `modbus.host`.
- `РЕГИСТР. НОМЕР` — controller ID on the GTC remote server. Irrelevant
  for Modbus TCP.
- `РЕГИСТР. КЛЮЧ` — registration key. Same — irrelevant for Modbus TCP.
  If this shows `0000`, the controller never reached `remoteaccess.gtcontrollers.com`
  successfully; that does **not** affect Modbus TCP, which works on the
  LAN segment regardless.

The panel also supports a manual IP path (Конфигурация → 7. СЕРВИС →
3. ETHERNET → 1. НАСТРОЙКА = Ручная, then 2. IP АДРЕС / 3. ОСНОВНОЙ
ШЛЮЗ) if DHCP gives an unreachable address. The Oasis Registers PDF
exposes the same fields at holding registers `0x15E–0x161` (IP) and
`0x162–0x164` (MAC) — see `Oasis_Registers.md`.

## 2. The Ethernet module does NOT block Modbus TCP

The PDF describes only the proprietary `GTC Remote Access` protocol over
the EM-LAN. Modbus TCP is not mentioned, but is confirmed by the
register map (Oasis Registers, addresses 0x15E–0x169 expose the EM-LAN
network configuration) and by independent reports of users running
Home Assistant `modbus.tcp` against `<controller-ip>:502` while keeping
the GTC mobile app open.

**Implication for our app:** `GTC_Control` can run alongside the
official mobile app without forcing the user to choose. If concurrent
sessions exceed the EM-LAN's capacity we will see TCP timeouts and
should treat `GTC_Control` as the primary client. Polling at 5–10 s
intervals (the default in `config/default.yml`) is well within the
"non-aggressive" range that has been reported to coexist with the
mobile app.

Nothing else in this PDF is relevant to the Rust implementation. The
RJ-45 cabling, GTC Remote Access account registration, and OS-specific
"how to find your default gateway" sections are all consumer-facing and
unrelated to Modbus.
