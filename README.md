# PowerScraper

PowerScraper is a high-performance, multithreaded Rust application designed to query metrics from solar inverters and energy meters, calculate battery charge/discharge commands dynamically to zero out grid interaction, and forward telemetry to monitoring platforms (EmonCMS and InfluxDB).

It operates on a fully decoupled, **MQTT-only inter-task communication architecture** for maximum task isolation and resilience.

---

## 1. Key Features

*   **Inverter Drivers**:
    *   **SolaX WiFi**: Queries real-time JSON endpoints on legacy WiFi dongles.
    *   **SolaX Modbus TCP (Standard)**: Connects to classic SolaX SK-SU5000E inverters.
    *   **SolaX X-Hybrid Modbus TCP**: Queries modern X-Series hybrid inverters using sparse block reads.
    *   **SolaX Generation 4 (G4) Modbus TCP**: Implements the VPP (Virtual Power Plant) 32-bit active power control rate interfaces.
    *   **SolaX Generation 3 (G3) Modbus TCP**: Implements G3-specific VPP 16-bit active power control rate overrides.
*   **Grid Meter Drivers**:
    *   **SDM630 Modbus RTU**: Isolated OS-threaded Modbus RTU serial driver with scheduling priority escapes (`SCHED_FIFO` / nice `-20`) to eliminate polling latency.
    *   **DTSU666 Modbus RTU**: High-priority serial driver for Chint energy meters.
    *   **MQTT Power Meter**: Bridges external MQTT meter topics (Shelly, ESPHome) to the regulation loop.
*   **Control Loop & Battery Coordination**:
    *   **Battery Grouping**: Proportionally balances charge/discharge requests based on SoC capacity headroom and BMS throttling states across multiple linked inverters.
    *   **Dynamic Grid Regulation**: Regulation loop automatically adjusts battery rates to track a configured grid target error (e.g. maintaining a target offset like `-50W`).
*   **Simulation & Parameter Tuning**:
    *   **Optimization Engine**: Includes Lookahead MPC, Adaptive Peak Shaving, and Evolved Heuristic simulation strategies.
    *   **Genetic Algorithm (GA) Tuning**: Automatically optimizes threshold parameters and solar forecast weightings against historical database telemetry.
*   **Web Dashboard & UI**:
    *   Built-in lightweight Actix-web server hosting status JSON API endpoints and an interactive web interface for real-time visualization and configuration.
    *   Static UI assets are packed directly inside the binary ([src/web_assets.rs](src/web_assets.rs)) for single-file deployments.
*   **System Reliability**:
    *   **WAL Mode SQLite Database**: Enforces Write-Ahead Logging and busy timeouts to safely query metrics and save configurations concurrently.
    *   **Hardware Watchdog**: Monitors MainsMeter activity and exits the process to allow systemd service restarts if communication links freeze.

---

## 2. Prerequisites & Dependencies

*   **Rust Toolchain**: Rust 2024 edition (`cargo` and `rustc`).
*   **System Libraries**: `libssl-dev` and `pkg-config` (required for HTTPS/SSL connections).
*   **External Broker**: An MQTT broker (such as `mosquitto`) is required for inter-task messaging.

```bash
# On Debian/Ubuntu systems:
sudo apt-get install build-essential libssl-dev pkg-config mosquitto
```

---

## 3. How to Build Manually

### 1. Compile Static Assets
If you have modified any files in the frontend web folder ([web/](web/)), you must recompile and pack them into the Rust source code using the python utility script:

```bash
python3 scripts/pack_assets.py
```

### 2. Compile the Binary
Build a release-optimized binary using standard Cargo profiles:

```bash
cargo build --release
```

The compiled binary will be available at `target/release/PowerScraper`.

---

## 4. Building Debian Packages

A helper script is provided to bundle the binary, systemd services, and configurations into a standard Debian package (`.deb`) for deployment.

The build script supports cross-compilation target arguments:
```bash
./scripts/build_deb.sh <architecture> <rust_target_triple>
```

### Example Configurations:

*   **Build locally (AMD64 / x86_64)**:
    ```bash
    ./scripts/build_deb.sh amd64 x86_64-unknown-linux-gnu
    ```
*   **Build for target system (ARM64 / aarch64)**:
    ```bash
    ./scripts/build_deb.sh arm64 aarch64-unknown-linux-gnu
    ```

The output package will be generated inside the `dist/` directory (e.g. `dist/powerscraper_1.0.68_arm64.deb`).

---

## 5. Deployment

Install the generated package on the target device:

```bash
sudo dpkg -i dist/powerscraper_1.0.68_arm64.deb
```

Start and enable the systemd daemon:

```bash
sudo systemctl daemon-reload
sudo systemctl enable powerscraper.service
sudo systemctl start powerscraper.service
```

---

## 6. Migration from Python Implementation

The new Rust implementation uses the same TOML structure and is fully backwards-compatible with the older Python `config.toml` file.

You can import your configuration using two different methods:

### Method A: Automatic Seeding on Startup (Recommended)
1. Copy your existing `config.toml` file to the root directory where `PowerScraper` is run.
2. Ensure there is no existing SQLite database file (`config.db`).
3. Start the application. PowerScraper will detect `config.toml`, parse it, and automatically seed and initialize the `config.db` database.

### Method B: Import via Web UI Settings
1. Start the application to initialize a blank database.
2. Open the Web Dashboard in your browser.
3. Navigate to **Settings** and click the **Import Config** button.
4. Select and upload your old `config.toml` file to parse and apply it to the database instantly.

---

## 7. Git Pre-Commit Hook

This repository enforces a Git pre-commit hook (`scripts/pre-commit`) to guarantee code quality. The hook automatically executes:
1. `cargo fmt` format validation.
2. `cargo clippy --all-targets -- -D warnings` lint check.
3. `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` documentation links validation.
4. Unit/integration test execution.
5. Increments the patch version in `Cargo.toml` and updates `Cargo.lock` with the synced version before staging both files.

