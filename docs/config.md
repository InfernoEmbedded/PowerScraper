# Configuration Guide

PowerScraper is configured via a single `config.toml` file located in the root of the workspace. This guide describes each configuration block, its parameters, and practical usage guidelines.

---

## 1. MQTT Broker `[MQTT]`

Defines the connection details for the central MQTT broker. All drivers publish telemetry, and the Power Manager listens to grid values and publishes inverter charging rate commands through this broker.

- `broker` (String, Required): IP address or hostname of the MQTT broker (e.g. `"127.0.0.1"`).
- `port` (Integer, Optional): Port of the broker. Defaults to `1883`.
- `base_topic` (String, Optional): Prefix for all PowerScraper topics. Defaults to `"sensors"`.
- `username` (String, Optional): Authentication username.
- `password` (String, Optional): Authentication password.
- `home-assistant-discovery` (Boolean, Optional): Automatically publish Home Assistant discovery configuration payloads on startup/observation. Defaults to `true`.
- `home-assistant-prefix` (String, Optional): Topic prefix used for Home Assistant discovery. Defaults to `"homeassistant"`.

Example:
```toml
[MQTT]
broker = "127.0.0.1"
port = 1883
base_topic = "sensors"
home-assistant-discovery = true
```

---

## 2. Inverter Drivers

### SolaX WiFi HTTP Driver `[Solax-Wifi]`
Used to scrape metrics from SolaX inverters via their local Wi-Fi dongle HTTP API.

- `poll_period` (Integer, Required): Polling interval in seconds.
- `timeout` (Integer, Required): HTTP request timeout in seconds.
- `inverters` (Array of Strings, Required): List of inverter IP addresses or hostnames (e.g., `["192.168.1.15"]`).

### SolaX Modbus TCP Driver `[Solax-Modbus]`
Used to scrape standard SolaX SK-SU5000E inverters over Modbus TCP.

- `poll_period` (Integer, Required): Polling interval in seconds.
- `timeout` (Integer, Required): Modbus communication timeout in seconds.
- `power_budget_avg_samples` (Integer, Optional): Number of samples to average the power budget over (defaults to `30`).
- `installer_password` (Integer, Optional): Installer password matching the front panel (required if performing battery rate control).
- `inverters` (Array of Strings, Required): Inverter identifier names.
- `hostnames` (Array of Strings, Optional): Modbus TCP host:port string mapping for each inverter (defaults to inverter name and port `502`).

### SolaX X-Hybrid Modbus TCP Driver `[Solax-XHybrid-Modbus]`
Used to scrape SolaX X-Series Hybrid inverters and chargers over Modbus TCP (uses divided register block queries).

- Same configuration parameters as standard `[Solax-Modbus]`.

---

## 3. Power Meter Drivers

### SDM630 Modbus RTU Meter `[SDM630Modbusv2]`
Used to query Eastron SDM630 Modbus RTU (RS485) energy meters.

- `poll_period` (Integer, Required): Polling interval in seconds.
- `timeout` (Integer, Required): Serial communication timeout in seconds.
- `baud` (Integer, Required): Baud rate (e.g., `38400`).
- `parity` (String, Required): Serial parity: `'N'` (None), `'E'` (Even), or `'O'` (Odd).
- `stopbits` (Integer, Required): Stop bits (`1` or `2`).
- `ports` (Array of Strings, Required): List of serial tty ports to monitor (e.g., `["/dev/ttyUSB0"]`).

### DTSU666 Modbus RTU Meter `[DTSU666]`
Used to query Chint DTSU666 Modbus RTU (RS485) energy meters.

- Same configuration parameters as `[SDM630Modbusv2]`.

### MQTT Power Meter Bridge `[MQTTPowerMeter]`
Bridges external MQTT power meter topics (e.g., Shelly, custom ESPHome) to the PowerScraper schema.

- `poll_period` (Integer, Optional): Default polling period for meter configurations.
- `meters` (Array of Strings, Required): Unique names for each meter bridge device.
- `[MQTTPowerMeter.<MeterName>]` (Sub-table, Required): Specific meter configuration:
  - `broker` (String, Required): IP/Hostname of the MQTT broker.
  - `port` (Integer, Optional): Broker port. Defaults to `1883`.
  - `username` (String, Optional): MQTT username.
  - `password` (String, Optional): MQTT password.
  - `topic-total` (String, Optional): Topic for total active power.
  - `topic-phase1` (String, Optional): Topic for Phase 1 active power.
  - `topic-phase2` (String, Optional): Topic for Phase 2 active power.
  - `topic-phase3` (String, Optional): Topic for Phase 3 active power.

Example:
```toml
[MQTTPowerMeter]
meters = ["MainsMeter"]

[MQTTPowerMeter.MainsMeter]
broker = "127.0.0.1"
port = 18830
topic-total = "shelly/em/total"
topic-phase1 = "shelly/em/p1"
```

---

## 4. Output Forwarders

### EmonCMS Forwarder `[emoncms]`
Forwards telemetry directly to an EmonCMS instance.

- `server` (String, Required): URL of the EmonCMS server.
- `api_key` (String, Required): EmonCMS write API key.
- `timeout` (Integer, Required): Network timeout in seconds.

### InfluxDB v1.8+ Forwarder `[influx]`
Forwards telemetry directly to InfluxDB using the v1 write protocol.

- `influx_url` (String, Required): URL of the InfluxDB server (e.g. `"http://localhost:8086"`).
- `influx_database` (String, Required): Target InfluxDB database.
- `influx_measurement` (String, Required): Measurement table.
- `influx_user` (String, Required): InfluxDB username.
- `influx_pass` (String, Required): InfluxDB password.
- `influx_retention_policy` (String, Required): Retention policy (e.g. `"autogen"`).

---

## 5. Battery Control & Power Manager `[Solax-BatteryControl]`

Controls charge and discharge rates of batteries across multiple periods.

- `source` (String, Optional): Name of the power meter that provides grid export/import telemetry.
- `linked-batteries` (Boolean, Optional): If `true`, charging/discharging rates are balanced equally across inverters.
- `timezone` (String, Optional): Specific IANA timezone for scheduled period calculations (e.g. `"Australia/Sydney"`).
- `grid-target` (Float, Optional): Target grid interactive power in Watts. Defaults to `0.0`. Negative values feed energy back into the grid (e.g., `-100.0` for a constant 100W feed-in).
- `initial-mode` (String, Optional): Starting mode for the Power Manager: `"Auto"`, `"ChargeBatteries"`, or `"MaximumFeedin"`. Defaults to `"Auto"`.

### Inverter Specific Constraints `[Solax-BatteryControl.inverter.<Name>]`
Configures physical constraints and routing rules for each participating inverter (where `<Name>` is the inverter identifier name from drivers).

- `phase` (Integer, Required): The grid phase the inverter is physically wired to (`1`, `2`, or `3`).
- `use-total-power` (Boolean, Optional): If `true`, the inverter regulates battery rates against the *total* system grid import/export, rather than just its own *phase* import/export.
- `single-phase-charge-limit` (Float, Optional): Threshold power in Watts. Below this, only regulate phase power; above this, regulate total system power.
- `single-phase-discharge-limit` (Float, Optional): Threshold power in Watts. Below this, only regulate phase power; above this, regulate total system power.
- `max-charge` (Float, Required): Hard ceiling for battery charging rate in Watts.
- `max-discharge` (Float, Required): Hard ceiling for battery discharging rate in Watts.
- `grace-capacity` (Integer, Optional): Battery Capacity % (SOC) at which charging stops early during grace periods (reserves capacity to absorb spikes).
- `grace-power-threshold` (Float, Optional): Capacity headroom is bypassed if solar PV output exceeds this threshold in Watts.
- `grace-charge-power` (Float, Optional): Power rate in Watts to charge at when capacity limits are active.
- `control-grid-power` (Boolean, Optional): Set to `true` for newer Gen3 inverters that regulate grid power directly, rather than battery power registers.

### Time-of-Use Periods `[Solax-BatteryControl.period.<PeriodName>]`
Configures time-of-use (TOU) scheduled windows.

- `start` (String, Required): Daily start time formatted as `"HH:MM:SS"`.
- `end` (String, Required): Daily end time formatted as `"HH:MM:SS"`.
- `min-charge` (Integer, Required): Minimum State of Charge (SOC) to maintain. The battery will not discharge below this.
- `grid-charge` (Boolean, Required): If `true`, charge the battery using grid power if the capacity drops below `min-charge`.
- `force-discharge` (Float, Optional): If specified, forces the battery to discharge at this rate in Watts during this period.
- `grace` (Boolean, Optional): Activates the `grace-capacity` settings for inverters during this period.
- `prefer-battery` (Boolean, Optional): Prioritize routing solar generation directly to charge the battery to its minimum threshold.
