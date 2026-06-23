# Power Meter Drivers Guide

PowerScraper supports multiple grid and phase power meters to feed consumption metrics to the Power Manager. Drivers are located in [src/drivers/](file:///home/deece/src/PowerScraper/src/drivers/).

---

## 1. Eastron SDM630 Modbus RTU Driver

The SDM630 driver connects to Eastron SDM630v2 three-phase utility meters over serial RS485 using Modbus RTU.

### Modbus Communication Details
- **Modbus Function**: Reads input registers (`FC 0x04`).
- **Data Format**: All parameters are returned as 32-bit IEEE 754 floating-point values across two consecutive 16-bit registers (Big-Endian).
- **Register Address Mapping**: The driver reads registers divided into 5 polling blocks:

| Address | Description | Unit |
| :--- | :--- | :--- |
| `0x0000` | Phase 1 Voltage | V |
| `0x0002` | Phase 2 Voltage | V |
| `0x0004` | Phase 3 Voltage | V |
| `0x0006` | Phase 1 Current | A |
| `0x0008` | Phase 2 Current | A |
| `0x000A` | Phase 3 Current | A |
| `0x000C` | Phase 1 Active Power | W |
| `0x000E` | Phase 2 Active Power | W |
| `0x0010` | Phase 3 Active Power | W |
| `0x0012` | Phase 1 Apparent Power | VA |
| `0x0014` | Phase 2 Apparent Power | VA |
| `0x0016` | Phase 3 Apparent Power | VA |
| `0x0018` | Phase 1 Reactive Power | var |
| `0x001A` | Phase 2 Reactive Power | var |
| `0x001C` | Phase 3 Reactive Power | var |
| `0x001E` | Phase 1 Power Factor | *None* |
| `0x0020` | Phase 2 Power Factor | *None* |
| `0x0022` | Phase 3 Power Factor | *None* |
| `0x003E` | Total System Active Power | W |
| `0x0046` | Frequency | Hz |
| `0x0048` | Import Active Energy | kWh |
| `0x004A` | Export Active Energy | kWh |
| `0x004C` | Import Reactive Energy | kvarh |
| `0x004E` | Export Reactive Energy | kvarh |

---

## 2. Chint DTSU666 Modbus RTU Driver

The DTSU666 driver monitors Chint DTSU666 three-phase power meters, which are frequently used alongside SolaX solar systems.

### Modbus Communication Details
- **Modbus Function**: Reads input registers (`FC 0x04`).
- **Data Format**: Identical to SDM630, returned as 32-bit float values.
- **Register Address Mapping**: Reads registers in 2 polling blocks:

| Address | Description | Unit |
| :--- | :--- | :--- |
| `0x2000` | Phase 1 Voltage | V |
| `0x2002` | Phase 2 Voltage | V |
| `0x2004` | Phase 3 Voltage | V |
| `0x2006` | Phase 1 Current | A |
| `0x2008` | Phase 2 Current | A |
| `0x200A` | Phase 3 Current | A |
| `0x200C` | Phase 1 Active Power | W |
| `0x200E` | Phase 2 Active Power | W |
| `0x2010` | Phase 3 Active Power | W |
| `0x2012` | Phase 1 Power Factor | *None* |
| `0x2014` | Phase 2 Power Factor | *None* |
| `0x2016` | Phase 3 Power Factor | *None* |
| `0x201E` | Total System Active Power | W |
| `0x2024` | Total Power Factor | *None* |
| `0x2026` | Frequency | Hz |
| `0x401E` | Import Active Energy | kWh |
| `0x4028` | Export Active Energy | kWh |
| `0x4032` | Import Reactive Energy | kvarh |
| `0x403C` | Export Reactive Energy | kvarh |
| `0x4046` | Total Q1-Q4 Reactive Energy (Quadrant) | kvarh |

---

## 3. MQTT Power Meter Bridge

For setups that do not utilize local physical serial connection (e.g. Shelly 3EM, ESPHome, or smart meters reporting via Home Assistant), PowerScraper includes an MQTT Power Meter Bridge.

- **Operation**: The bridge client subscribes to custom configured MQTT topics on the broker.
- **Translation Loop**: When a value is published to a bridged topic, the bridge client intercepts the payload, parses the numerical representation, maps it to the standard PowerScraper schema, and publishes it back to the central broker under the base topic:

```
sensors/{meter_name}/Total system power
sensors/{meter_name}/Phase 1 power
sensors/{meter_name}/Phase 2 power
sensors/{meter_name}/Phase 3 power
```

This translation decouples external telemetry endpoints, enabling the Power Manager to run its regulation calculations identically regardless of whether the meter is physical RS485 or external MQTT.
