# Solax Hybrid X1/X3 Generation 3 (G3) Modbus Protocol Specification

This document details the Modbus communication protocol for the Solax Hybrid X1 and X3 Generation 3 inverters.

---

## 1. General Protocol Parameters

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

---

## 2. Telemetry (Read Input Registers - Function Code `0x04`)

Generation 3 inverters divide the input registers into non-contiguous blocks. Querying registers in a single contiguous block of `0x72` registers may cause modbus exception errors (Illegal Data Address) on certain models because registers between blocks are undefined. It is recommended to query telemetry in three separate blocks:

### Block A (Address `0x0000` to `0x0026` - Basic Telemetry)
*   `0x0000` (0): `GridVoltage` (0.1V, `uint16`)
*   `0x0001` (1): `GridCurrent` (0.1A, `int16`)
*   `0x0002` (2): `GridPower` (1W, `int16`)
*   `0x0003` (3): `PvVoltage1` (0.1V, `uint16`)
*   `0x0004` (4): `PvVoltage2` (0.1V, `uint16`)
*   `0x0005` (5): `PvCurrent1` (0.1A, `uint16`)
*   `0x0006` (6): `PvCurrent2` (0.1A, `uint16`)
*   `0x0007` (7): `GridFrequency` (0.01Hz, `uint16`)
*   `0x0008` (8): `Temperature` (1℃, `int16`)
*   `0x0009` (9): `RunMode` (Operating state, `uint16`)
*   `0x000A` (10): `Powerdc1` (PV1 Power, 1W, `uint16`)
*   `0x000B` (11): `Powerdc2` (PV2 Power, 1W, `uint16`)
*   `0x0014` (20): `BatVoltage_Charge1` (Battery Voltage, 0.1V, `int16`)
*   `0x0015` (21): `BatCurrent_Charge1` (Battery Current, 0.1A, `int16`)
*   `0x0016` (22): `Batpower_Charge1` (Battery Power, 1W, `int16`, positive = charge, negative = discharge)
*   `0x0017` (23): `BMS_Connect_State` (BMS Status, `uint16`)
*   `0x0018` (24): `TemperatureBat` (Battery Temp, 1℃, `int16`)
*   `0x001C` (28): `Battery Capacity` (SOC %, `uint16`)

### Block B (Address `0x0040` to `0x005D` - Grid Metrics)
*   `0x0040` (64): `Inverter Fault` (LSB/MSB, `uint32` split registers)
*   `0x0043` (67): `Manager Fault` (`uint16`)
*   `0x0046` (70): `Measured Power` (Grid Meter Power, 1W, `int32` split registers)
*   `0x0048` (72): `Feed In Energy` (Total grid export, 0.01kWh, `uint32` split registers)
*   `0x004A` (74): `Consumed Energy` (Total grid import, 0.01kWh, `uint32` split registers)
*   `0x004C` (76): `Off-Grid Voltage` (0.1V, `uint16`)
*   `0x004D` (77): `Off-Grid Current` (0.1A, `uint16`)
*   `0x004E` (78): `Off-Grid Power` (1VA, `uint16`)
*   `0x0050` (80): `Energy Today` (0.1kWh, `uint16`)
*   `0x0052` (82): `Energy Total` (0.1kWh, `uint32` split registers)
*   `0x0055` (85): `Battery Temperature` (0.1℃, `uint16`)

### Block C (Address `0x006A` to `0x0078` - Three-Phase Grid Details)
*   Phase-specific grid telemetry (voltages, currents, reactive powers).

---

## 3. Remote Power Control (Write Single Registers - Function Code `0x06`)

VPP control registers for SolaX Gen 3 inverters:

### 1. Control Mode (`0x0051` - `ModbusPowerControl`)
Enables remote override control.
*   **Address**: `0x0051` (81 Dec)
*   **Format**: `uint16` (Write Single Register - `0x06`)
*   **Values**:
    *   `0`: Disable remote override control (reverts to standard self-use mode)
    *   `1`: Enable remote active/reactive power rate control

### 2. Active Power Command (`0x0052` - `Modbus ActivePower`)
Configures the battery charge/discharge rate target in Watts.
*   **Address**: `0x0052` (82 Dec)
*   **Format**: `int16` (Write Single Register - `0x06`, cast negative discharge rates to two's complement `u16` before writing)
*   **Sign Convention**:
    *   **Positive values (`> 0`)**: Commands the battery to **charge** from the grid/inverter.
    *   **Negative values (`< 0`)**: Commands the battery to **discharge** into the house/grid.

### 3. Keepalive Timeout (`0x009F` - `PowerControl_timeout`)
*   **Address**: `0x009F` (159 Dec)
*   **Format**: `uint16` (seconds, range `5` to `65535`)
*   **Description**: Watchdog timer for remote control. Reverts the inverter back to self-use mode if no active writes are received within this period. A value of `30` (seconds) is recommended.
