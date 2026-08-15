# Changelog

All notable user-facing changes in PowerScraper since the transition from the legacy Python implementation (commit `2c2227e5fbb7957aed1c77ad66eb0986f63ecfbf`) are documented below.

---

## [1.0.156] - 2026-08-15

### Added
* **Solax Inverter Advanced Configuration (Gen 3 & V2.50)**:
  - Added full read/write support for all Modbus Function Code `0x06` writable registers for Solax Gen 3 (V3.21) and older V2.50 inverters.
  - Dynamically generate configuration UI based on driver capabilities to ensure 100% exposure of writable hardware parameters.
  - Implemented an auto-generated "**⚙️ Other Settings**" section for any unmapped write-only registers.
  - Fixed API omissions to force-expose writable registers even if omitted from core telemetry reads.
* **UI Improvements**:
  - Restructured Charge and Discharge Window End time inputs to their own rows for better legibility.

---

## [1.0.120] - 2026-08-10

### Added
* **Configurable Auto Mode Energy Cost Margin Control**:
  - Added running calculation of stored battery energy unit cost based on charging sources (export price for solar charging, import price for grid charging).
  - Added `auto_cost_margin` (`auto-cost-margin`) configuration parameter to `SolaxBatteryControlConfig` (c/kWh).
  - Configured Auto mode regulation in both `PowerManager` live control and historical simulation (`auto.rs`) to suppress battery discharging whenever stored battery unit cost + margin exceeds the current grid import price.

---

## [1.0.119] - 2026-08-03

### Added
* **Nagios / NEMS Compatible Health API Endpoints (`/health` & `/api/health`)**:
  - Added REST endpoints that check telemetry freshness across all configured battery inverters.
  - Returns `HTTP 200 OK` (`"status": "healthy"`) when all battery inverters are online and updated within the last 60 seconds.
  - Returns `HTTP 503 Service Unavailable` (`"status": "unhealthy"`) when any configured battery inverter is offline or stale (>60s since telemetry update), triggering standard Nagios `check_http` CRITICAL alerts.
  - Returns detailed JSON payload containing status, epoch timestamp, total battery inverter count, healthy inverter count, and per-inverter update metrics.
* **Nagios & NEMS Linux Integration**:
  - Configured and verified active service check `PowerScraper Health` on NEMS Linux server (`root@monitor.lan`).

---

## [1.0.106] - 2026-08-01

### Added
* **History Chart Redesign & Unit Standardization**:
  - Standardized Solar Generation, Battery Power, Grid Power, and Household Usage history graphs to display in **kW** instead of Watts.
  - Converted Battery Capacity history graph from percentage (%) to stored energy in **kWh** using inferred capacity or configured battery rating.
  - Added stacked per-PV array rendering (PV1, PV2) on Solar Generation graph.
  - Added split color coding on Grid Power graph: **Green** for feed-in (<0 kW) and **Red** for grid draw (>0 kW).
  - Added smooth **Green-to-Red** color gradation to Household Usage graph.
  - Added conditional hiding of PV array details in Location tab and History charts when an inverter has 'no pv' enabled.
* **Server-Side Telemetry Decimation & Profiling**:
  - Implemented server-side mean-average bucket decimation using the `max_pixels` parameter to avoid over-fetching telemetry data.
  - Added microsecond profiling logs and `Server-Timing` HTTP response headers (`db`, `decimate`, `format`, `total`).
* **Automatic Pre-Fetch Telemetry Flush**:
  - Added thread-safe `PENDING_HISTORY` buffer in `database.rs`.
  - Automatically flushes all pending in-memory telemetry to SQLite DB prior to serving `/api/history` queries.
  - Flushes pending telemetry to SQLite on process shutdown, signal receipt (SIGTERM, SIGINT), and via `libc::atexit` hook.

### Optimized
* **Database Schema & Indexing**:
  - Split `telemetry_history` topic into `device` and `field` columns with compound indices `idx_telemetry_history_field_timestamp` and `idx_telemetry_history_device_field_timestamp`.
  - Added non-blocking batched schema migration (`LIMIT 50000`) on startup to prevent SQLite database locks.
  - Replaced serde_json allocations with zero-copy `TelemetryRecordRef` serialization, reducing `/api/history` query duration from >4.6s to ~30ms (**~160x speedup**).

---

## [1.0.79] - 2026-07-31

### Fixed
* **SolaX Inverter Remote Control & Sign Alignments**:
  - Aligned Gen 3 (`solax_g3.rs`) command writing for Holding Register `0x0052` with the `0.x` Python `SolaxXHybridModbus` implementation (passing target power as two's complement without extra negation).
  - Fixed battery power telemetry sign reporting across `solax_g3.rs` and `solax_g4.rs` by negating Input Register `0x0016` (`-i16_a(0x16)`), ensuring telemetry maps to PowerScraper's system-wide convention (`battery_power > 0` = Discharging).
  - Restored battery power telemetry negation in `solax_modbus.rs` (`-signed16_a(0x16)`) for Gen 2 SK-SU inverters.
  - Corrected 32-bit Modbus word ordering (`high_word`, `low_word`) for `ActivePowerTarget` (`0x007E`) in `solax_g4.rs`.
* **Modbus Quiet Time & Rate Limiting**: Added 200ms quiet-time inter-frame delays between consecutive Modbus write instructions to prevent packet drops.

### Added
* **Inverter Identification Documentation**: Added hardware generation matrix and serial prefix lookup table (`docs/inverters.md`).

---

## [1.0.78] - 2026-07-03

### Added
*   **Solax X-Hybrid Telemetry Expansion**: Added parsing for missing Modbus input registers on X-Hybrid inverters (including Bus Voltage, DC Voltage Fault, Overload Fault, Battery Voltage Fault, BMS Connected, and Run Mode 2).

---

## [1.0.77] - 2026-07-03

### Fixed
*   **EmonCMS Payload Serialization**: Parse stringified metric values to their correct JSON numeric (integers, floats) and boolean types before submitting. This resolves the issue where EmonCMS rejected the payloads and stopped updating feeds.

---

## [1.0.76] - 2026-07-02

### Added
*   **Web-Configurable Battery Charge Hysteresis**: Added global and period-specific hysteresis options to prevent battery charging/discharging hunting (oscillation) near the minimum SOC limits (addresses issue #23).
*   **Hysteresis State Machine & Simulation Validation**: Added a stateful `low_capacity_state` tracker in the live `PowerManager` loop and aligned the simulation run loop in `src/simulation/auto.rs`.
*   **Automated Tests for Hysteresis**: Implemented comprehensive unit tests verifying state transitions of hysteresis logic, simulation engine accuracy, and Playwright UI config reload verification.

---

## [1.0.75] - 2026-07-01

### Added
*   **Systemd Notify/Watchdog Protocol**: Implemented native `sd_notify` integration using raw Unix datagram sockets. The daemon now signals `READY=1` after startup, pings `WATCHDOG=1` every 30 seconds when telemetry is healthy, and sends `STOPPING=1` on clean shutdown — enabling systemd to automatically detect and restart deadlocked or frozen processes.
*   **SQLite WAL Checkpointing**: Added `PRAGMA wal_checkpoint(TRUNCATE)` after config saves and telemetry flushes to compact the WAL file and prevent unbounded storage growth on embedded targets.

### Changed
*   **Systemd Service Type**: Changed from `Type=simple` to `Type=notify` with `WatchdogSec=180` in the packaged service file.
*   **Database Directory Permissions**: Tightened `/var/lib/powerscraper` from `755` to `750` to protect stored MQTT credentials and API tokens.

---

## [1.0.74] - 2026-07-01

### Added
*   **REST API oneshot testing**: Extracted Axum `Router` builder into `build_web_app` to allow rigorous programmatic requests testing (400 Bad Request, 404 Not Found, 415 Unsupported Media Type, and 422 Unprocessable Entity payload handling).
*   **Database Write Error Fail-Safe tests**: Expanded unit testing in `src/database.rs` to verify rusqlite write failure recovery by passing directory paths to SQLite connections.
*   **E2E Playwright test expansions**: Added Test Cases 11 through 15 verifying historical simulation result cards, genetic algorithm model parameters tuning via SSE progress stream, dynamic capacity calculations on PV arrays, solar orientation inference correlation grid, and tariff Time-Of-Use periods.
*   **Mock Telemetry Seeding**: Updated test server startup sequence to generate and seed a 1440-record double-peak household load, solar yield curve, battery charging, and flat electricity pricing data to enable simulation E2E tests.

---

## [1.0.73] - 2026-07-01

### Added
*   **Test Coverage Expansion**: Added extensive unit and integration tests across core configurations, tariff management, EmonCMS and InfluxDB telemetry forwarders, solar orientation inference, and evolved heuristics. Evaluated and justified untestable boundaries.

---

## [1.0.70] - 2026-06-30

### Added
*   **SolaX Generation 4 (G4) Modbus Driver**: Custom driver supporting the 32-bit Virtual Power Plant (VPP) active power holding registers (`0x007C`, `0x007E`, `0x0088`).
*   **SolaX Generation 3 (G3) Modbus Driver**: Custom driver supporting 16-bit remote control overrides and watchdog keepalives using sparse input block queries to avoid illegal address exceptions.
*   **Mains Meter Watchdog Config**: Watchdog timeout settings are now exposed directly on the Web UI Settings tab and saved to the database.
*   **Python Configuration Migration**: Added automated TOML-to-database seeding on startup and an **Import Config** file upload button in the Web Dashboard for seamless migration.
*   **Pre-commit Hook Auto-Sync**: The pre-commit hook now runs `cargo metadata` and stages `Cargo.lock` automatically to ensure version consistency.

### Changed
*   **Top-up Period Charge Targets**: Min-SoC charging logic now evaluates target thresholds as the maximum of the period's min-charge and the inverter's baseline hard floor limit (e.g., proper grid charging behavior during TOU windows).

---

## [1.0.50] - 2026-06-25

### Added
*   **Realtime Modbus RTU Threading**: Moved SDM630 and DTSU666 serial drivers to dedicated OS threads running with realtime priority (`SCHED_FIFO` / nice `-20`) to eliminate grid telemetry latency.
*   **Lookahead MPC Reserve**: Added support for grid pre-charging under demand tariffs in simulation.
*   **SQLite Database Layer**: Factored out all direct query logic into a repository module leveraging WAL (Write-Ahead Logging) mode and busy timeouts.

### Changed
*   **Tuning Execution**: Parallelized the evolutionary parameter optimization in native Rust, replacing the slow Python child process executor.

---

## [1.0.0] - 2026-06-20

### Added
*   **Full Rust Port**: Rewrote the entire backend from Python/Twisted to Rust/Tokio, achieving a 10x reduction in memory footprint and massive performance enhancements.
*   **Advanced Simulation Engine**: Native simulator with support for Lookahead MPC, Adaptive Peak Shaving, and Evolved Heuristic strategies.
*   **Web Dashboard UI**: A premium, responsive single-page dashboard displaying live power flows, system stats, Amber pricing, and historical simulation graphs.
*   **MQTT Power Meter Bridge**: Support for Shelly and custom ESPHome meters.
*   **Home Assistant Auto-Discovery**: Automatically publishes MQTT discovery payloads for all entities.
*   **Tariff Manager**: Live pricing models supporting Flat rates, Time-Of-Use schedules, and real-time Amber API price tracking.
*   **Debian Packaging**: Integrated building scripts for packaging `.deb` files for local (`amd64`) or target (`arm64`) deployments.
