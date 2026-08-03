# PowerScraper REST API & Debug Endpoint Documentation

PowerScraper includes a built-in REST API and Web UI server running on port `3000` (configurable) that exposes real-time system status, configuration management, telemetry export, and diagnostic endpoints.

---

## Debug Endpoints (`GET /api/debug` & `GET /debug`)

The `/api/debug` (and shorthand `/debug`) endpoints provide a complete diagnostic overview of the running PowerScraper daemon. They combine active driver data source timestamps, full database configuration, live system status, and internal `PowerManager` control state into a single JSON payload.

### Request

```bash
curl -s http://power.lan:3000/api/debug | jq .
```

### JSON Response Schema

| Field Name | Type | Description |
| :--- | :--- | :--- |
| `timestamp` | `string` | ISO-8601 UTC timestamp of the debug response |
| `timestamp_epoch` | `integer` | Unix timestamp (seconds since Unix epoch) |
| `config` | `object` | The active database configuration struct (`Config`) |
| `system_status` | `object` | Live `SystemStatus` object including current mode, grid target, and inverter state |
| `data_sources` | `array` | Breakdown of every configured data source, its type, latest values, and `last_updated` epoch timestamp |
| `power_manager_internal_state` | `object` | Internal state of `PowerManager`, including mode, grid target, inverter requested charge/discharge rates, price thresholds, power budget, and power budget with charging |

### Payload Structure Example

```json
{
  "timestamp": "2026-08-02T22:32:26.205408165+00:00",
  "timestamp_epoch": 1785709946,
  "config": {
    "Solax-BatteryControl": {
      "grid-target": -50.0,
      "initial-mode": "Auto",
      "source": "MainsMeter"
    }
  },
  "system_status": {
    "active_mode": "Auto",
    "grid_target": -50.0,
    "import_price": 20.38493,
    "export_price": -9.21725,
    "meter_power": -76.164,
    "meter_last_updated": 1785709945,
    "inverters": {
      "solax-x1": {
        "battery_capacity": 20,
        "battery_power": -19,
        "pv_power": 0,
        "requested_power": -644,
        "run_mode": 2,
        "last_updated": 1785709946,
        "calculated_battery_capacity": 9.62
      }
    }
  },
  "data_sources": [
    {
      "name": "solax-x1",
      "type": "solax_g3_modbus",
      "status": {
        "battery_capacity": 20,
        "battery_power": -19,
        "pv_power": 0,
        "requested_power": -644,
        "last_updated": 1785709946
      }
    },
    {
      "name": "/dev/ttyMainsMeter",
      "type": "sdm630_modbus_v2",
      "meter_power": -76.164,
      "meter_last_updated": 1785709945
    }
  ],
  "power_manager_internal_state": {
    "active_mode": "Auto",
    "grid_target": -50.0,
    "import_price": 20.38493,
    "export_price": -9.21725,
    "meter_power": -76.164,
    "power_budget": 12500.0,
    "power_budget_with_charging": 14500.0
  }
}
```

---

## Standard REST API Endpoints

### 1. `GET /api/status`
Returns current system status overview formatted for the dashboard:
- `active_mode`: Active power manager mode (e.g. `"Auto"`, `"SmartHeuristic"`)
- `grid_target`: Configured grid target power in Watts (e.g. `-50.0`)
- `meter_power`: Mains meter reading in Watts
- `meter_last_updated`: Unix timestamp of last meter telemetry update
- `import_price` & `export_price`: Active electricity tariff rates
- `inverters`: Map of configured inverters with SOC, PV power, battery power, and commanded `requested_power`

### 2. `GET /api/config`
Retrieves the full running configuration in JSON format.

### 3. `POST /api/config`
Updates configuration in the SQLite database and triggers an internal daemon reload signal. Expects a JSON `Config` object body.

### 4. `POST /api/config/import`
Imports a TOML string configuration payload and updates daemon state.

### 5. `GET /api/config/export`
Exports running configuration as TOML formatted plain text.

### 6. `GET /api/backup/export` & `GET /api/backup/download`
Generates a complete system backup JSON object containing configuration and historical database telemetry. The `/api/download` route serves an XZ-compressed binary stream with `Content-Disposition` header.

### 7. `POST /api/backup/import`
Restores system state from an uncompressed JSON or XZ-compressed backup file stream.

### 8. `GET /api/telemetry/export.csv`
Streams full historical telemetry from SQLite as a downloadable CSV dataset (`Content-Type: text/csv`).

### 9. `POST /api/mqtt/test`
Tests connection parameters to an MQTT broker with given credentials and base topic, returning a diagnostic success or error summary.
