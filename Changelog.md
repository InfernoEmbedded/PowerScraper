# Changelog

All notable user-facing changes in PowerScraper since the transition from the legacy Python implementation (commit `2c2227e5fbb7957aed1c77ad66eb0986f63ecfbf`) are documented below.

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
