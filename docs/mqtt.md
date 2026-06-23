# MQTT Integration & Home Assistant Discovery

PowerScraper uses a central MQTT broker to decouple drivers, power manager, and forwarders. This document details the topic structure, payloads, and the Home Assistant Auto-Discovery integration.

---

## 1. MQTT Topic Specifications

The prefix `{base_topic}` refers to the configured `base_topic` parameter in the `[MQTT]` section (defaults to `"sensors"`).

### Telemetry Topics (Status)
Drivers publish measurements periodically to the following topics:

| Topic Pattern | Description | Payload Format |
| :--- | :--- | :--- |
| `{base_topic}/{device_name}/{metric}` | Raw telemetry metric values | String (Float/Integer) |
| `{base_topic}/power_manager/mode` | Current active mode of the Power Manager | `"Auto"`, `"ChargeBatteries"`, or `"MaximumFeedin"` |
| `{base_topic}/power_manager/grid_target` | Current target interactive grid power | String (Float) |
| `{base_topic}/{inverter_name}/Requested Battery Power` | Current battery control power commanded by the Power Manager | String (Integer, Watts) |

### Control Topics (Commands)
External clients or the Power Manager publish control commands to:

| Topic Pattern | Description | Allowed Payloads |
| :--- | :--- | :--- |
| `{base_topic}/{inverter_name}/command/charge_battery` | Sets target charging power for standard/hybrid inverters | String (Integer, Watts): Positive to charge; negative to discharge |
| `{base_topic}/power_manager/command/mode` | Sets the Power Manager operating mode | `"auto"`, `"chargebatteries"`, `"maximumfeedin"` (case-insensitive) |
| `{base_topic}/power_manager/command/grid_target` | Sets the Power Manager grid target power in Watts | String (Float, e.g. `0.0`, `-100.0`) |

---

## 2. Home Assistant Auto-Discovery

If `home-assistant-discovery` is enabled (default), PowerScraper automatically registers its topics with Home Assistant. Discovery payloads are published with the `retain = true` flag.

### Discovery Topic Structure
Discovery config payloads are published to the following pattern:
```
<discovery_prefix>/<component>/<device_sanitized>/<entity_sanitized>/config
```
- `<discovery_prefix>`: Defaults to `"homeassistant"`.
- `<component>`: Matches `sensor` (telemetry), `number` (numeric inputs), or `select` (dropdown options).
- `<device_sanitized>`: Lowercase lowercase alphanumeric representation of the device ID (e.g. `"solax_modbus"`).
- `<entity_sanitized>`: Sanitized unique entity identifier (e.g. `"pv1_voltage"`).

### Device Registry Grouping
To prevent cluttering, all entities created for a single physical inverter or meter are grouped under a unified Device Registry entry in Home Assistant. This is achieved by publishing a shared `device` block inside all configuration payloads:

```json
{
  "identifiers": ["powerscraper_solax_modbus"],
  "name": "solax-modbus",
  "model": "PowerScraper Device",
  "manufacturer": "PowerScraper"
}
```

### Entity Classification Rules

PowerScraper dynamically inspects metric names and attaches the appropriate `unit_of_measurement`, `device_class`, and `state_class` settings:

| Metric Keyword | Component | Device Class | Unit | State Class |
| :--- | :--- | :--- | :--- | :--- |
| `"Voltage"`, `"v_phase"` | `sensor` | `voltage` | `V` | `measurement` |
| `"Current"`, `"a_phase"` | `sensor` | `current` | `A` | `measurement` |
| `"Frequency"` | `sensor` | `frequency` | `Hz` | `measurement` |
| `"Temp"`, `"Temperature"` | `sensor` | `temperature` | `°C` | `measurement` |
| `"Capacity"`, `"SOC"`, `"SOH"` | `sensor` | `battery` | `%` | `measurement` |
| `"kvarh"` | `sensor` | *None* | `kvarh` | `total_increasing` |
| `"kvar"` | `sensor` | `reactive_power` | `var` | `measurement` |
| `"VA"` | `sensor` | `apparent_power` | `VA` | `measurement` |
| `"Energy"`, `"Today"`, `"Total"` | `sensor` | `energy` | `kWh` | `total_increasing` |
| `"Power"`, `"Budget"`, `"Usage"` | `sensor` | `power` | `W` | `measurement` |

### Control Entity Config Payloads

#### Power Manager Mode (`select`)
- Discovery Topic: `homeassistant/select/power_manager/mode/config`
- Payload:
  ```json
  {
    "name": "Mode",
    "state_topic": "sensors/power_manager/mode",
    "command_topic": "sensors/power_manager/command/mode",
    "unique_id": "powerscraper_power_manager_mode",
    "options": ["Auto", "ChargeBatteries", "MaximumFeedin"],
    "device": { ... }
  }
  ```

#### Power Manager Grid Target (`number`)
- Discovery Topic: `homeassistant/number/power_manager/grid_target/config`
- Payload:
  ```json
  {
    "name": "Grid Target",
    "state_topic": "sensors/power_manager/grid_target",
    "command_topic": "sensors/power_manager/command/grid_target",
    "unique_id": "powerscraper_power_manager_grid_target",
    "min": -10000,
    "max": 10000,
    "step": 1,
    "unit_of_measurement": "W",
    "device_class": "power",
    "device": { ... }
  }
  ```

#### Inverter Charge Battery Command (`number`)
- Discovery Topic: `homeassistant/number/{inverter_name}/charge_battery/config`
- Payload:
  ```json
  {
    "name": "{inverter_name} Charge Battery",
    "state_topic": "sensors/{inverter_name}/Requested Battery Power",
    "command_topic": "sensors/{inverter_name}/command/charge_battery",
    "unique_id": "powerscraper_{inverter_name}_charge_battery",
    "min": -10000,
    "max": 10000,
    "step": 1,
    "unit_of_measurement": "W",
    "device_class": "power",
    "device": { ... }
  }
  ```
