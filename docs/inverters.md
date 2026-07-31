# Inverter Drivers Guide

PowerScraper supports multiple communication drivers to read metrics and command charging rates for SolaX inverters. These drivers reside in [src/drivers/](file:///home/deece/src/PowerScraper/src/drivers/).

---

## 1. SolaX WiFi HTTP Driver

The Wi-Fi driver scrapes SolaX inverters by issuing periodic HTTP requests to the inverter's Wi-Fi dongle local IP.

- **Polling Endpoint**: `http://{inverter_host}/api/realTimeData.htm`
- **JSON Pre-processing**: SolaX Wi-Fi modules often produce malformed JSON strings containing double commas (e.g. `,,`) when specific data blocks fail to resolve. The driver automatically strips and replaces these with `,0,` before performing deserialization.
- **Metric Array Map**: The SolaX JSON response returns a raw `Data` array of values. PowerScraper parses this array using the following index map:

| Index | Metric Name | Description |
| :--- | :--- | :--- |
| `0` | `PV1 Current` | Solar PV Phase 1 Current (A) |
| `1` | `PV2 Current` | Solar PV Phase 2 Current (A) |
| `2` | `PV1 Voltage` | Solar PV Phase 1 Voltage (V) |
| `3` | `PV2 Voltage` | Solar PV Phase 2 Voltage (V) |
| `4` | `Grid Current` | Export/Import Grid Current (A) |
| `5` | `Grid Voltage` | Grid AC Voltage (V) |
| `6` | `Grid Power` | Total grid interactive power (W) |
| `7` | `Inner Temp` | Inverter internal temperature (°C) |
| `8` | `Solar Today` | Accumulated generation today (kWh) |
| `9` | `Solar Total` | Lifetime generation (kWh) |
| `10` | `Feed In Power` | Feed-in active power (W) |
| `11` | `PV1 Power` | PV Phase 1 Generation (W) |
| `12` | `PV2 Power` | PV Phase 2 Generation (W) |
| `13` | `Battery Voltage` | DC battery terminal voltage (V) |
| `14` | `Battery Current` | DC battery terminal current (A) |
| `15` | `Battery Power` | Net battery charge/discharge active power (W) |
| `16` | `Battery Temp` | Battery pack internal temperature (°C) |
| `17` | `Battery Capacity` | State of Charge (SOC %) |
| `19` | `Solar Total 2` | Alternative generation counter (kWh) |
| `41` | `Energy to Grid` | Lifetime grid export total (kWh) |
| `42` | `Energy from Grid`| Lifetime grid import total (kWh) |
| `50` | `Grid Frequency` | AC utility grid frequency (Hz) |
| `53` | `EPS Voltage` | Emergency power supply output voltage (V) |
| `54` | `EPS Current` | Emergency power supply output current (A) |
| `55` | `EPS VA` | Apparent EPS power demand (VA) |
| `56` | `EPS Frequency` | Emergency power supply output frequency (Hz) |
| `67` | `Status` | Inverter operating state enumeration code |

---

## 2. SolaX Modbus TCP Driver (Standard)

The standard Modbus driver is designed for classic SolaX SK-SU5000E inverters. It connects to the inverter over TCP (typically port `502`) and performs input register read requests (`FC 0x04`).

### Input Registers (Read-Only Telemetry)
The driver issues a single block read of `0x72` registers starting at base address `0`. Key registers parsed include:

- `0x00`: Grid Voltage (`u16`, scaled by `0.1`)
- `0x01`: Grid Current (`i16`, scaled by `0.1`)
- `0x02`: Inverter Active Power (`i16`)
- `0x03` - `0x04`: PV1/PV2 Voltage (`u16`, scaled by `0.1`)
- `0x05` - `0x06`: PV1/PV2 Current (`u16`, scaled by `0.1`)
- `0x07`: Grid Frequency (`u16`, scaled by `0.01`)
- `0x08`: Inner Temperature (`i16`)
- `0x09`: Operating Run Mode (`u16`)
- `0x0a` - `0x0b`: PV1/PV2 Power (`u16`)
- `0x14`: Battery Voltage (`i16`, scaled by `0.01`)
- `0x15`: Battery Current (`i16`, scaled by `0.01`)
- `0x16`: Battery Power (`i16`, negative = charge, positive = discharge)
- `0x17` - `0x19`: Charger board/battery/boost temperatures (`i16`)
- `0x1C`: State of Charge (`u16`, %)
- `0x1D`: Accumulated Battery Energy Discharged (`u32` split registers, scaled by `0.1`)
- `0x20`: Accumulated Battery Energy Charged (`u32` split registers, scaled by `0.1`)
- `0x23`: Battery State of Health (SOH, `u16`, %)
- `0x46`: Grid Meter Measured Power (`i32` split registers)
- `0x48`: Lifetime Feed-in Energy (`u32` split registers, scaled by `0.01`)
- `0x4A`: Lifetime Consumed Energy (`u32` split registers, scaled by `0.01`)
- `0x50`: Inverter Energy Today (`u16`, scaled by `0.1`)
- `0x52`: Lifetime Inverter Energy Total (`u32` split registers, scaled by `0.001`)

### Holding Registers (Control Commands)
When the driver receives a target rate instruction on `{base_topic}/{inverter_name}/command/charge_battery`, it issues:
1. `write_single_register(0x51, power_value)`: Sets the target battery power register (positive value = charge, negative value = discharge, converted to its two's complement equivalent for negative numbers).
2. `write_single_register(0x90, 1)`: Triggers/flushes the control write command validation on the inverter board.

---

## 3. SolaX X-Hybrid Modbus TCP Driver

The X-Hybrid Modbus TCP driver queries modern SolaX hybrid charger inverters. Because registers are sparse and separated into distant addresses, the driver issues three distinct Modbus read blocks:

1. **Block A**: Reads `0x27` input registers starting at base address `0` (basic PV power and battery parameters).
2. **Block B**: Reads registers `0x40` through `0x69` (system states, faults, energy counters).
3. **Block C**: Reads registers `0x6A` through `0xCD` (detailed three-phase grid measurements, phase voltages, currents).

### Handshake Sequence
Modern SolaX hybrid firmware requires a startup handshake to enable remote battery command overrides. The driver performs the handshake automatically upon connection:
1. `write_single_register(0x00, installer_password)`: Authorizes installer access.
2. `write_single_register(0x9F, 30)`: Sets the remote control watchdog timeout (seconds).
3. `write_single_register(0x51, 1)`: Enables remote command control overrides.
4. `write_single_register(0x53, 0)`: Disables local/standard charging profile schedules.
5. `write_single_register(0x40, 3)`: Puts the inverter manager in "Remote Control" override state.

### Holding Registers (Control Commands)
When commanding battery rates:
1. `write_single_register(0x52, power_value)`: Sets the target battery charge/discharge rate.
2. `write_single_register(0x51, 1)`: Ensures the control state override remains active.

---

## 4. SolaX Generation 4 (G4) Modbus TCP Driver

The Generation 4 (G4) driver is designed for modern SolaX Hybrid X1/X3 Gen 4 inverters. Telemetry registers match the standard single-block read of `0x72` registers starting at address `0` (similar to the standard driver).

### Remote Power Control
G4 inverters utilize a dedicated Virtual Power Plant (VPP) remote control interface:
1. **Enable Remote Control** (`0x007C`): Write Single Register (`0x06`) set to `1` (remote control enabled) or `0` (revert to self-use).
2. **Keepalive Timeout** (`0x0088`): Write Single Register (`0x06`) setting the watchdog timeout in seconds (typically `30`).
3. **Active Power Target** (`0x007E`): Write Multiple Registers (`0x10`, length 2) containing a 32-bit signed integer (low word at `0x007E`, high word at `0x007F`).
   - Positive values charge the battery.
   - Negative values discharge the battery.

---

## 5. SolaX Generation 3 (G3) Modbus TCP Driver

The Generation 3 (G3) driver is designed specifically for SolaX Hybrid X1/X3 Gen 3 inverters. To prevent illegal address exception errors on sparse G3 registers, the driver splits telemetry queries into three non-contiguous block reads:
- **Block A**: Reads `0x27` registers starting at `0x00` (PV power, battery telemetry).
- **Block B**: Reads `0x1E` registers starting at `0x40` (fault states, grid meter).
- **Block C**: Reads `0x0E` registers starting at `0x6A` (three-phase measurements).

### Remote Power Control
G3 VPP remote override control utilizes the following holding registers:
1. **Enable Remote Control** (`0x0051`): Write Single Register (`0x06`) set to `1` (remote control enabled) or `0` (revert to self-use).
2. **Keepalive Timeout** (`0x009F`): Write Single Register (`0x06`) setting the watchdog timeout in seconds (typically `30`).
3. **Active Power Target** (`0x0052`): Write Single Register (`0x06`) containing a signed 16-bit power value (positive values charge, negative values discharge).

---

## 6. Inverter Hardware Generation & Serial Prefix Identification

SolaX encodes hardware generation, phase count, and series type in the leading characters of the 14-character ASCII Serial Number read at holding register `0x0000`:

| Serial Prefix | Hardware Generation | Inverter Series / Type | Driver Class | Remote Control Registers |
| :--- | :--- | :--- | :--- | :--- |
| **`U50...`** / **`U30...`** | **Gen 2 (SK-SU)** | SK-SU 3000 / SK-SU 5000E Single Phase | `Solax-Modbus` | `0x0051` (Power), `0x0090` (Trigger) |
| **`PR...`** / **`PRI...`** | **Gen 3 (G3)** | X1-Hybrid / X3-Hybrid Gen 3 | `Solax-G3-Modbus` | `0x0051` (Enable=1), `0x009F` (Timeout=30), `0x0052` (Power Int16) |
| **`H1...`** / **`H3...`** | **Gen 4 (G4)** | X1-Hybrid / X3-Hybrid Gen 4 | `Solax-G4-Modbus` | `0x007C` (Enable=1), `0x0088` (Timeout=30), `0x007E` (Power Int32) |
| **`X3...`** / **`MIC...`** | Grid-Tied | X3-Mic / X1-Boost (No Battery) | `Solax-Modbus` | N/A (String Inverters) |

