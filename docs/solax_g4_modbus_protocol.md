# Solax Hybrid X1/X3 Generation 4 (G4) Modbus Protocol Specification

This document details the Modbus communication protocol for the Solax Hybrid X1 and X3 Generation 4 inverters.

---

## 1. General Protocol Parameters

The inverter supports communication using **Modbus RTU** (over RS485 serial ports) and **Modbus TCP** (via the monitoring interface module).

*   **Modbus RTU Parameters**:
    *   **Baud Rate**: `19200` bps (default)
    *   **Data Bits**: `8`
    *   **Stop Bits**: `1`
    *   **Parity**: `None`
    *   **Unit ID / Slave Address**: `1` (default)
*   **Modbus TCP Parameters**:
    *   **Port**: `502`
    *   **Unit ID**: `1` (default)
*   **Timing Requirements**:
    *   **Least Interval between Instructions**: `1 Sec`
    *   **Character-gap Timeout**: `> 100ms`
    *   **Response Timeout**: `1 Sec`
*   **Data Representation**:
    *   **32-bit registers**: Sent in little-endian word format (low-order 16-bit word at the lower address; high-order 16-bit word at the higher address).

> [!WARNING]
> Frequent writing of registers marked as saved to EEPROM (marked with `★` or `EE` in some manual versions) will cause irreversible hardware damage due to EEPROM write cycle limitations. Avoid periodic writes to configuration registers.

---

## 2. Telemetry (Read Input Registers - Function Code `0x04`)

These registers represent read-only status and telemetry data, queried with function code `0x04`.

| Address (Hex) | Address (Dec) | Variable Name | Unit | Data Type | Description |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `0x0000` | 0 | `GridVoltage` | `0.1V` | `uint16` | Grid voltage (Single phase or Phase R) |
| `0x0001` | 1 | `GridCurrent` | `0.1A` | `int16` | Grid current (Single phase or Phase R) |
| `0x0002` | 2 | `GridPower` | `1W` | `int16` | Grid power (positive = export, negative = import) |
| `0x0003` | 3 | `PvVoltage1` | `0.1V` | `uint16` | PV string 1 voltage |
| `0x0004` | 4 | `PvVoltage2` | `0.1V` | `uint16` | PV string 2 voltage |
| `0x0005` | 5 | `PvCurrent1` | `0.1A` | `uint16` | PV string 1 current |
| `0x0006` | 6 | `PvCurrent2` | `0.1A` | `uint16` | PV string 2 current |
| `0x0007` | 7 | `GridFrequency` | `0.01Hz` | `uint16` | Grid frequency |
| `0x0008` | 8 | `Temperature` | `1℃` | `int16` | Radiator temperature |
| `0x0009` | 9 | `RunMode` | `-` | `uint16` | Operating mode (Self-use, Force-charge, etc.) |
| `0x000A` | 10 | `Powerdc1` | `1W` | `uint16` | PV1 input power |
| `0x000B` | 11 | `Powerdc2` | `1W` | `uint16` | PV2 input power |
| `0x0014` | 20 | `BatVoltage_Charge1` | `0.1V` | `int16` | Battery pack voltage |
| `0x0015` | 21 | `BatCurrent_Charge1` | `0.1A` | `int16` | Battery pack current (positive = charge, negative = discharge) |
| `0x0016` | 22 | `Batpower_Charge1` | `1W` | `int16` | Battery pack power (positive = charge, negative = discharge) |
| `0x0017` | 23 | `BMS_Connect_State` | `-` | `uint16` | BMS connection state (`0`: disconnected, `1`: connected) |
| `0x0018` | 24 | `TemperatureBat` | `1℃` | `int16` | Battery pack temperature |
| `0x0019` | 25 | `BDCStatus` | `-` | `uint16` | battery charger status (`0`: discharge, `1`: charge, `2`: stop) |
| `0x001A` | 26 | `GridStatus` | `-` | `uint16` | Grid connection status (`0`: on-grid, `1`: off-grid) |
| `0x001C` | 28 | `Battery Capacity` | `1%` | `uint16` | Battery State of Charge (SoC) |
| `0x001D` | 29 | `OutputEnergy_Charge.LSB` | `0.1kWh` | `uint32` (2 regs) | Cumulative battery discharge energy (LSB) |
| `0x001E` | 30 | `OutputEnergy_Charge.MSB` | `0.1kWh` | `uint32` (2 regs) | Cumulative battery discharge energy (MSB) |
| `0x0020` | 32 | `OutputEnergy_Charge_today`| `0.1kWh` | `uint16` | Battery discharge energy today |
| `0x0021` | 33 | `InputEnergy_Charge.LSB` | `0.1kWh` | `uint32` (2 regs) | Cumulative battery charge energy (LSB) |
| `0x0022` | 34 | `InputEnergy_Charge.MSB` | `0.1kWh` | `uint32` (2 regs) | Cumulative battery charge energy (MSB) |
| `0x0023` | 35 | `InputEnergy_Charge_today` | `0.1kWh` | `uint16` | Battery charge energy today |
| `0x0024` | 36 | `BMS ChargeMaxCurrent` | `0.1A` | `uint16` | Maximum allowed BMS charge current (real-time) |
| `0x0025` | 37 | `BMS DischargeMaxCurrent` | `0.1A` | `uint16` | Maximum allowed BMS discharge current (real-time) |
| `0x0026` | 38 | `BMS_BatteryCapacity` | `1Wh` | `uint32` (2 regs) | BMS battery pack capacity |
| `0x0046` | 70 | `feedin_power` | `1W` | `int32` (2 regs) | Net grid import/export power (positive = export/feedin, negative = import/consume) |
| `0x0048` | 72 | `feedin_energy_total` | `0.01kWh` | `uint32` (2 regs) | Total feedin energy to grid |
| `0x004A` | 74 | `consum_energy_total` | `0.01kWh` | `uint32` (2 regs) | Total consumed energy from grid |
| `0x004C` | 76 | `Off-gridVoltage` | `0.1V` | `uint16` | Backup off-grid voltage |
| `0x004D` | 77 | `Off-gridCurrent` | `0.1A` | `uint16` | Backup off-grid current |
| `0x004E` | 78 | `Off-gridPower` | `1VA` | `uint16` | Backup off-grid output power |
| `0x004F` | 79 | `Off-gridFrequency` | `0.01Hz` | `uint16` | Backup off-grid frequency |
| `0x0050` | 80 | `Etoday_togrid` | `0.1kWh` | `uint16` | Today's exported energy |
| `0x0052` | 82 | `Etotal_togrid` | `0.1kWh` | `uint32` (2 regs) | Total exported energy |
| `0x0055` | 85 | `Battery Temperature` | `0.1℃` | `uint16` | Auxiliary battery temperature |
| `0x0070` | 112 | `Solar Energy Total` | `0.1kWh` | `uint32` (2 regs) | Total solar generation energy |

---

## 3. Remote Power Control (Write Single/Multiple Registers)

For remote Virtual Power Plant (VPP) and power rate control, the following registers are used. These do **not** write to EEPROM and can be updated frequently.

### 1. Control Mode (`0x007C` - `ModbusPowerControl`)
Enables or disables remote modbus power control.
*   **Address**: `0x007C` (124 Dec)
*   **Format**: `uint16` (Write Single Register - `0x06`)
*   **Values**:
    *   `0`: Disable remote control (default, returns to self-use)
    *   `1`: Enable remote active/reactive power rate control
    *   `2`: Enable electric quantity control
    *   `3`: Enable SOC target control

### 2. Active Power Command (`0x007E` - `RemoteControl ActivePower`)
Configures the charge/discharge rate target in Watts.
*   **Address**: `0x007E` (126 Dec, spans `0x007E` to `0x007F`)
*   **Format**: `int32` (Little Endian, write using Write Multiple Registers - `0x10`)
*   **Sign Convention**:
    *   **Positive values (`> 0`)**: Commands the battery to **charge** from the grid/inverter.
    *   **Negative values (`< 0`)**: Commands the battery to **discharge** into the house/grid.
*   **Example**: To command `1500W` charging, write `1500` (`0x000005DC`). To command `2000W` discharging, write `-2000` (`0xFFFFF830`).

### 3. Keepalive Timeout (`0x0088` - `RemoteCtrlTimeOut`)
Ensures remote control safely falls back if the host controller goes offline.
*   **Address**: `0x0088` (136 Dec)
*   **Format**: `uint16` (seconds)
*   **Description**: Timeout counter. The inverter will count down from this value. If it hits zero before receiving another Modbus power write, it disables remote control and reverts to self-use mode. A value of `30` (seconds) is recommended.

---

## 4. Example Modbus Handshakes

### Enabling Remote Discharging at 2000W:
1.  **Enable Remote Control (ModbusPowerControl)**:
    *   Write `1` to `0x007C`
    *   `01 06 00 7C 00 01 [CRC_L] [CRC_H]`
2.  **Set Timeout (RemoteCtrlTimeOut)**:
    *   Write `30` (seconds) to `0x0088`
    *   `01 06 00 88 00 1E [CRC_L] [CRC_H]`
3.  **Command -2000W Discharge (RemoteControl ActivePower)**:
    *   Write `-2000` (`0xFFFFF830`) as `int32` at `0x007E`
    *   Low word (`0xF830`) to `0x007E`, High word (`0xFFFF`) to `0x007F`
    *   Write Multiple Registers (`0x10`):
    *   `01 10 00 7E 00 02 04 F8 30 FF FF [CRC_L] [CRC_H]`
