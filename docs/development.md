# PowerScraper Development Guide

This guide details how to set up the development, testing, and deployment environment for the PowerScraper project.

---

## 1. Project Architecture

The project consists of three main components:
- **Rust Backend Daemon** (`src/`): Evaluates regulation constraints, talks to standard Modbus RTU/TCP & XHybrid inverters, exposes a web API, and publishes telemetry/commands to MQTT.
- **Web Frontend** (`web/`): A clean, premium dashboard built using HTML/CSS and Vanilla JS.
- **Test Suite** (`tests/`):
  - Rust integration tests (`tests/integration_test.rs`) utilizing a mock Mosquitto MQTT broker.
  - Playwright browser integration tests (`tests/web_ui_integration.js`) that exercise the web interface.

---

## 2. Prerequisites

Ensure the following tools are installed on your development machine:

### Backend Development
- **Rust Toolchain**: [rustup](https://rustup.rs/) (edition 2021)
- **Mosquitto Broker**: Used for MQTT messaging tests.
  ```bash
  sudo apt-get install mosquitto mosquitto-clients
  ```

### Frontend & UI Testing
- **Node.js**: Version 16+ (with npm)
- **Playwright**: Browser automation library.
  ```bash
  cd tests/
  npm install playwright
  npx playwright install --with-deps
  ```

---

## 3. Local Development Flow

### Compiling the Code
To build the daemon in debug mode:
```bash
cargo build
```

### Static Asset Packaging
The web UI files (`web/index.html`, `web/app.js`, `web/style.css`) are embedded directly into the Rust binary. If you modify any frontend files, you must repackage them before compiling:
```bash
python3 scripts/pack_assets.py
```
This updates [src/web_assets.rs](file:///home/deece/watt-home/src/PowerScraper/src/web_assets.rs).

---

## 4. Running the Test Suite

### Process-Isolated Testing with `cargo-nextest`
We recommend using **`cargo-nextest`** to execute tests. It runs each unit and integration test in its own isolated process, preventing port collisions and cross-talk.

1. **Install nextest**:
   ```bash
   cargo install --locked cargo-nextest
   ```
2. **Add cargo bin to path** (if not already present):
   ```bash
   export PATH="$HOME/.cargo/bin:$PATH"
   ```
3. **Execute nextest**:
   ```bash
   cargo nextest run
   ```

### Web UI Integration Testing (Playwright)
To verify that the web dashboard compiles, renders alphanumeric-sorted telemetry, handles configuration edits, and updates instant modes correctly:
```bash
node tests/web_ui_integration.js
```
*(This starts a test server on port 3000, launches a headless browser, and validates all UI features).*

---

## 5. Packaging & Deployment

To compile and package the daemon as a Debian release (`.deb`):

### Cross-compiling for target hardware (e.g. Raspberry Pi / ARM64):
```bash
./scripts/build_deb.sh arm64 aarch64-unknown-linux-gnu
```
This generates the package at [dist/powerscraper_0.1.0_arm64.deb](file:///home/deece/watt-home/src/PowerScraper/dist/powerscraper_0.1.0_arm64.deb).

### Installing on Target (e.g., `power.lan`):
1. Transfer the built package:
   ```bash
   scp dist/powerscraper_0.1.0_arm64.deb root@power.lan:/tmp/
   ```
2. SSH into target and install:
   ```bash
   ssh root@power.lan "dpkg -i /tmp/powerscraper_0.1.0_arm64.deb && systemctl restart powerscraper"
   ```
3. Monitor live log updates:
   ```bash
   ssh root@power.lan "journalctl -u powerscraper.service -f"
   ```
