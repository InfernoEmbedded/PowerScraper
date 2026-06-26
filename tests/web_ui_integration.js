const { spawn, execSync } = require('child_process');
const { chromium } = require('playwright');
const path = require('path');
const fs = require('fs');

async function main() {
    console.log("=== Web UI Integration Test Suite ===");

    // Step 1: Compile the test server
    console.log("Building web_ui_test_server binary...");
    try {
        execSync('cargo build --bin web_ui_test_server', { stdio: 'inherit' });
    } catch (e) {
        console.error("Failed to build web_ui_test_server", e);
        process.exit(1);
    }

    // Step 2: Spawn test server in the background
    console.log("Starting test server...");
    const serverProcess = spawn('target/debug/web_ui_test_server', [], {
        stdio: 'inherit',
        detached: false
    });

    // Register cleanup for server process
    const cleanup = () => {
        console.log("Cleaning up resources...");
        try {
            serverProcess.kill();
        } catch (e) {}
        if (fs.existsSync('test_web_ui.db')) {
            try {
                fs.unlinkSync('test_web_ui.db');
            } catch (e) {}
        }
    };

    process.on('exit', cleanup);
    process.on('SIGINT', () => { cleanup(); process.exit(1); });
    process.on('SIGTERM', () => { cleanup(); process.exit(1); });

    // Step 3: Poll http://localhost:3000/ until ready
    console.log("Waiting for web server to start on port 3000...");
    let serverReady = false;
    for (let i = 0; i < 30; i++) {
        try {
            const resp = await fetch('http://localhost:3000/');
            if (resp.ok) {
                serverReady = true;
                break;
            }
        } catch (e) {
            // ignore
        }
        await new Promise(r => setTimeout(r, 500));
    }

    if (!serverReady) {
        console.error("Web server failed to start within timeout.");
        cleanup();
        process.exit(1);
    }
    console.log("Web server is ready! Launching Playwright browser...");

    // Step 4: Launch browser
    let browser;
    try {
        // Try launching playwright default chromium first
        browser = await chromium.launch({ headless: true });
    } catch (e) {
        console.log("Failed to launch default playwright chromium. Falling back to system google-chrome...");
        try {
            browser = await chromium.launch({
                headless: true,
                executablePath: '/usr/bin/google-chrome'
            });
        } catch (err) {
            console.error("Failed to launch fallback browser", err);
            cleanup();
            process.exit(1);
        }
    }

    const context = await browser.newContext();
    const page = await context.newPage();

    // Dialog handler to capture and accept alerts
    let dialogText = null;
    page.on('dialog', async dialog => {
        dialogText = dialog.message();
        console.log(`[Browser Alert]: ${dialogText}`);
        await dialog.accept();
    });

    try {
        // Test Case 1: Page Load & Dashboard Telemetry
        console.log("Running Test 1: Page Load & Dashboard Telemetry...");
        await page.goto('http://localhost:3000/');
        
        // Wait for title
        await page.waitForSelector('#page-title');
        const title = await page.innerText('#page-title');
        if (title !== "System Dashboard") {
            throw new Error(`Expected page title to be 'System Dashboard', got '${title}'`);
        }

        // Verify Mains Grid Power telemetry value
        // Note: meter_power = -250.5, which rounds to -251 W or -250 W depending on js rounding
        const mainsVal = await page.innerText('#stat-mains');
        if (!mainsVal.includes('25') || !mainsVal.includes('W') || !mainsVal.includes('Feedin')) {
            throw new Error(`Expected Mains Grid Power to represent approx 250 W Feedin, got '${mainsVal}'`);
        }

        // Verify grid power card title is "Current Grid Power"
        const gridPowerTitle = await page.evaluate(() => {
            const el = document.getElementById('stat-mains');
            return el ? el.closest('.glass-card').querySelector('.card-title').innerText.trim() : '';
        });
        if (gridPowerTitle !== "Current Grid Power") {
            throw new Error(`Expected Grid Power card title to be 'Current Grid Power', got '${gridPowerTitle}'`);
        }

        // Verify active mode
        const modeVal = await page.innerText('#stat-mode');
        if (modeVal !== "Auto") {
            throw new Error(`Expected active mode to be 'Auto', got '${modeVal}'`);
        }

        // Verify grid target
        const targetVal = await page.innerText('#stat-target');
        if (targetVal !== "100 W Draw") {
            throw new Error(`Expected grid target to be '100 W Draw', got '${targetVal}'`);
        }

        // Verify inverter telemetry rendering
        await page.waitForSelector('.inverter-item');
        const inverterElements = await page.$$('.inverter-item');
        if (inverterElements.length !== 2) {
            throw new Error(`Expected 2 inverter elements, found ${inverterElements.length}`);
        }
        
        const inverterText = await page.innerText('#dash-inverters-list');
        if (!inverterText.includes('solax-modbus') || !inverterText.includes('85 %') || !inverterText.includes('solax-xhybrid')) {
            throw new Error(`Inverter telemetry rendered incorrectly: '${inverterText}'`);
        }

        // Verify the telemetry is sorted alphabetically by inverter names/IDs
        const renderedIds = await page.evaluate(() => {
            const items = Array.from(document.querySelectorAll('.inverter-item'));
            return items.map(item => {
                const idEl = item.querySelector('.inverter-field-val');
                return idEl ? idEl.innerText.trim() : '';
            });
        });
        if (renderedIds[0] !== 'solax-modbus' || renderedIds[1] !== 'solax-xhybrid') {
            throw new Error(`Expected inverters to be sorted alphabetically, got: ${JSON.stringify(renderedIds)}`);
        }

        // Verify tariff prices are rendered
        const importPriceText = await page.innerText('#stat-import-price');
        if (importPriceText !== "28.5 c/kWh") {
            throw new Error(`Expected import price to be '28.5 c/kWh', got '${importPriceText}'`);
        }
        const exportPriceText = await page.innerText('#stat-export-price');
        if (exportPriceText !== "-8.2 c/kWh") {
            throw new Error(`Expected export price to be '-8.2 c/kWh', got '${exportPriceText}'`);
        }

        console.log("Test 1 Passed successfully!");

        // Test Case 2: Tab Navigation
        console.log("Running Test 2: Tab Navigation...");
        const tabs = [
            { btnText: 'MQTT Settings', expectedTitle: 'MQTT Settings', tabId: 'tab-mqtt' },
            { btnText: 'Hardware Drivers', expectedTitle: 'Hardware Drivers', tabId: 'tab-hardware' },
            { btnText: 'Battery Control', expectedTitle: 'Battery Control', tabId: 'tab-battery' },
            { btnText: 'TOU Periods', expectedTitle: 'TOU Periods', tabId: 'tab-periods' },
            { btnText: 'Forwarders', expectedTitle: 'Forwarders', tabId: 'tab-forwarders' },
            { btnText: 'Electricity Tariff', expectedTitle: 'Electricity Tariff', tabId: 'tab-tariff' },
            { btnText: 'Simulation', expectedTitle: 'Simulation', tabId: 'tab-simulation' },
            { btnText: 'About', expectedTitle: 'About', tabId: 'tab-about' },
            { btnText: 'Dashboard', expectedTitle: 'System Dashboard', tabId: 'tab-dashboard' }
        ];

        for (const tab of tabs) {
            console.log(`Switching to tab: ${tab.btnText}`);
            const btn = page.locator(`.nav-btn:has-text("${tab.btnText}")`);
            await btn.click();
            await page.waitForTimeout(100);

            // Assert page title updates
            const curTitle = await page.innerText('#page-title');
            if (curTitle !== tab.expectedTitle) {
                throw new Error(`Expected page title to be '${tab.expectedTitle}', got '${curTitle}'`);
            }

            // Assert correct tab content element is active/visible
            const classes = await page.getAttribute(`#${tab.tabId}`, 'class');
            if (!classes.includes('active')) {
                throw new Error(`Expected tab content #${tab.tabId} to have 'active' class`);
            }
        }
        console.log("Test 2 Passed successfully!");

        // Test Case 3: Instant Mode Controls
        console.log("Running Test 3: Instant Mode Controls...");
        // Return to dashboard
        await page.click('.nav-btn:has-text("Dashboard")');
        
        // Click Charge Batteries mode card
        console.log("Clicking 'Charge Batteries' mode card...");
        await page.click('#mode-charge');
        await page.waitForTimeout(500);

        // Verify telemetry active mode updates (which fetches system status post update)
        // Wait up to 10 seconds for polling/fetching updates
        await page.waitForFunction(() => document.getElementById('stat-mode').innerText === 'ChargeBatteries', { timeout: 10000 });
        console.log("Telemetry active mode updated to 'ChargeBatteries'!");
        
        const isChargeActive = await page.evaluate(() => document.getElementById('mode-charge').classList.contains('active'));
        if (!isChargeActive) {
            throw new Error("Expected #mode-charge card to have 'active' class");
        }

        // Click MPC Arbitrage mode card
        console.log("Clicking 'MPC Arbitrage' mode card...");
        await page.click('#mode-mpc-arbitrage');
        await page.waitForTimeout(500);

        // Verify telemetry active mode updates
        await page.waitForFunction(() => document.getElementById('stat-mode').innerText === 'MpcArbitrage', { timeout: 10000 });
        console.log("Telemetry active mode updated to 'MpcArbitrage'!");

        const isArbActive = await page.evaluate(() => document.getElementById('mode-mpc-arbitrage').classList.contains('active'));
        if (!isArbActive) {
            throw new Error("Expected #mode-mpc-arbitrage card to have 'active' class");
        }

        console.log("Test 3 Passed successfully!");

        // Test Case 4: Instant Grid Target Input & Set
        console.log("Running Test 4: Instant Grid Target Input...");
        // Set instant target value input
        await page.fill('#instant-target-val', '-450');
        await page.click('button:has-text("Set Target")');
        await page.waitForTimeout(500);

        // Verify telemetry target updates
        await page.waitForFunction(() => document.getElementById('stat-target').innerText === '450 W Feedin', { timeout: 10000 });
        console.log("Telemetry target updated to '450 W Feedin'!");
        console.log("Test 4 Passed successfully!");

        // Test Case 5: Configuration Form Editing and Saving
        console.log("Running Test 5: Configuration Form Editing & Saving...");
        // Navigate to MQTT tab
        await page.click('.nav-btn:has-text("MQTT")');
        
        // Fill out some details
        console.log("Updating MQTT Broker host and port...");
        await page.fill('#mqtt-broker', 'test.mqtt.server');
        await page.fill('#mqtt-port', '8883');
        await page.fill('#mqtt-base', 'my-test-sensors');

        // Setup dialog expectation
        dialogText = null;
        console.log("Clicking 'Apply Changes'...");
        await page.click('.btn-apply');
        
        // Wait for standard reload and confirmation alert in Node context
        let waitCount5 = 0;
        while (dialogText === null && waitCount5 < 50) {
            await page.waitForTimeout(100);
            waitCount5++;
        }
        if (dialogText === null) {
            throw new Error("Timed out waiting for confirmation alert");
        }
        if (!dialogText.includes("Configuration updated and live reloaded successfully")) {
            throw new Error(`Unexpected confirmation alert text: ${dialogText}`);
        }

        // Verify values are kept on config reload
        const brokerVal = await page.inputValue('#mqtt-broker');
        const portVal = await page.inputValue('#mqtt-port');
        const baseVal = await page.inputValue('#mqtt-base');
        if (brokerVal !== 'test.mqtt.server' || portVal !== '8883' || baseVal !== 'my-test-sensors') {
            throw new Error(`Config values not saved correctly: broker=${brokerVal}, port=${portVal}, base=${baseVal}`);
        }
        console.log("Test 5 Passed successfully!");

        // Test Case 6: Configuration Import
        console.log("Running Test 6: Configuration Import...");
        dialogText = null;

        // Clear configuration in test to make 'Import Config' button visible
        console.log("Clearing config to verify 'Import Config' visibility...");
        await page.evaluate(async () => {
            await fetch('/api/config', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    MQTT: {
                        broker: "127.0.0.1",
                        port: 1883,
                        "base-topic": "sensors",
                        "home-assistant-discovery": true,
                        "home-assistant-prefix": "homeassistant"
                    }
                })
            });
            await loadConfig();
        });
        await page.waitForTimeout(500);
        
        // We trigger file input upload
        console.log("Importing 'tests/test_config.toml'...");
        const fileChooserPromise = page.waitForEvent('filechooser');
        await page.click('.btn-import');
        const fileChooser = await fileChooserPromise;
        await fileChooser.setFiles(path.join(__dirname, 'test_config.toml'));

        // Wait for success alert dialog in Node context
        let waitCount6 = 0;
        while (dialogText === null && waitCount6 < 50) {
            await page.waitForTimeout(100);
            waitCount6++;
        }
        if (dialogText === null) {
            throw new Error("Timed out waiting for import success alert");
        }
        if (!dialogText.includes("Configuration imported successfully")) {
            throw new Error(`Unexpected import alert text: ${dialogText}`);
        }

        // Verify MQTT broker and port were loaded from test_config.toml (127.0.0.1, 18830)
        const importedBroker = await page.inputValue('#mqtt-broker');
        const importedPort = await page.inputValue('#mqtt-port');
        if (importedBroker !== '127.0.0.1' || importedPort !== '18830') {
            throw new Error(`Imported config values incorrect: broker=${importedBroker}, port=${importedPort}`);
        }

        // Verify that 'Import Config' button is now hidden because config is populated
        const importBtnVisible = await page.evaluate(() => {
            const btn = document.querySelector('.btn-import');
            return btn ? btn.style.display !== 'none' : false;
        });
        if (importBtnVisible) {
            throw new Error("Expected 'Import Config' button to be hidden after successful import");
        }

        console.log("Test 6 Passed successfully!");

        // Test Case 7: Hardware Drivers dynamic blocks and modal
        console.log("Running Test 7: Hardware Drivers dynamic blocks and modal...");
        // 1. Navigate to Hardware Drivers tab
        await page.click('.nav-btn:has-text("Hardware Drivers")');
        await page.waitForTimeout(500);

        // 2. Assert that we have cards rendered (loaded from test_config.toml or db)
        // From test_config.toml, we should have 4 cards initially
        let driverCards = await page.$$('.driver-card');
        if (driverCards.length !== 4) {
            throw new Error(`Expected 4 initial driver cards, found ${driverCards.length}`);
        }

        // 3. Click "+ Add Driver" button
        await page.click('button:has-text("+ Add Driver")');
        await page.waitForSelector('#add-driver-modal', { state: 'visible' });

        // 4. Select "Solax-Wifi" and click Add
        await page.selectOption('#new-driver-type', 'Solax-Wifi');
        await page.click('#add-driver-modal button:has-text("Add")');
        await page.waitForSelector('#add-driver-modal', { state: 'hidden' });

        // 5. Verify 5 cards now
        driverCards = await page.$$('.driver-card');
        if (driverCards.length !== 5) {
            throw new Error(`Expected 5 driver cards after adding one, found ${driverCards.length}`);
        }

        // 6. Set the hostname on the newly added Wifi driver
        const lastCard = driverCards[driverCards.length - 1];
        const wifiHostInput = await lastCard.$('.driver-wifi-host');
        await wifiHostInput.fill('192.168.1.155:80');

        // 7. Click Apply Changes to save
        dialogText = null;
        await page.click('.btn-apply');
        
        let waitCount7 = 0;
        while (dialogText === null && waitCount7 < 50) {
            await page.waitForTimeout(100);
            waitCount7++;
        }
        if (dialogText === null) {
            throw new Error("Timed out waiting for Apply Changes alert");
        }
        
        // 8. Verify the 5 cards are still there and the last one has the correct host
        await page.click('.nav-btn:has-text("Hardware Drivers")');
        driverCards = await page.$$('.driver-card');
        if (driverCards.length !== 5) {
            throw new Error(`Expected 5 driver cards after config reload, found ${driverCards.length}`);
        }
        
        // Find the card by checking its inputs
        const savedHosts = [];
        for (const card of driverCards) {
            const wifiInput = await card.$('.driver-wifi-host');
            if (wifiInput) {
                const val = await wifiInput.inputValue();
                savedHosts.push(val);
            }
        }
        if (!savedHosts.includes('192.168.1.155:80')) {
            throw new Error(`Expected saved Wifi host '192.168.1.155:80' to be in the list, found: [${savedHosts.join(', ')}]`);
        }

        // 9. Remove the card we just added (find the card with the value '192.168.1.155:80')
        let cardToRemove = null;
        for (const card of driverCards) {
            const wifiInput = await card.$('.driver-wifi-host');
            if (wifiInput) {
                const val = await wifiInput.inputValue();
                if (val === '192.168.1.155:80') {
                    cardToRemove = card;
                    break;
                }
            }
        }
        if (!cardToRemove) {
            throw new Error("Could not find the added driver card to remove");
        }
        const removeBtn = await cardToRemove.$('.delete-btn');
        await removeBtn.click();

        // 10. Verify 4 cards in DOM
        driverCards = await page.$$('.driver-card');
        if (driverCards.length !== 4) {
            throw new Error(`Expected 4 driver cards after removal, found ${driverCards.length}`);
        }

        // 11. Click Apply Changes again
        dialogText = null;
        await page.click('.btn-apply');
        
        let waitCount8 = 0;
        while (dialogText === null && waitCount8 < 50) {
            await page.waitForTimeout(100);
            waitCount8++;
        }
        if (dialogText === null) {
            throw new Error("Timed out waiting for Apply Changes alert after delete");
        }

        // 12. Verify 4 cards are still there
        await page.click('.nav-btn:has-text("Hardware Drivers")');
        driverCards = await page.$$('.driver-card');
        if (driverCards.length !== 4) {
            throw new Error(`Expected 4 driver cards after reload following delete, found ${driverCards.length}`);
        }
        console.log("Test 7 Passed successfully!");


        // Test Case 8: Full Configuration Options Walkthrough (Exercising and saving all possible configuration options)
        console.log("Running Test 8: Full Configuration Options Walkthrough...");

        // 1. MQTT settings
        console.log("Filling MQTT tab...");
        await page.click('.nav-btn:has-text("MQTT Settings")');
        await page.fill('#mqtt-broker', 'my-mqtt-broker.local');
        await page.fill('#mqtt-port', '1883');
        await page.fill('#mqtt-username', 'test-user');
        await page.fill('#mqtt-password', 'test-password');
        await page.fill('#mqtt-base', 'my-home-sensors');
        await page.check('#mqtt-ha-discovery');
        await page.fill('#mqtt-ha-prefix', 'homeassistant-custom');

        // 2. Hardware Drivers (fill values for existing drivers)
        console.log("Filling Hardware Drivers tab...");
        await page.click('.nav-btn:has-text("Hardware Drivers")');
        // Let's modify the standard Modbus driver hostname/port/timeout
        const modbusHostInput = page.locator('.driver-card[data-driver-type="Solax-Modbus"] .driver-modbus-host').first();
        await modbusHostInput.fill('192.168.1.100:502');
        const modbusPollInput = page.locator('.driver-card[data-driver-type="Solax-Modbus"] .driver-modbus-poll').first();
        await modbusPollInput.fill('5');

        // 3. Battery Control (General options & Add Inverter constraint)
        console.log("Filling Battery Control tab...");
        await page.click('.nav-btn:has-text("Battery Control")');
        await page.check('#battery-enable');
        await page.fill('#battery-source', 'SolaX-Hybrid-Meter');
        await page.fill('#battery-tz', 'Australia/Sydney');
        await page.fill('#battery-grid-target', '150');
        await page.selectOption('#battery-init-mode', 'MpcArbitrage');
        await page.check('#battery-linked');
        
        // Add inverter constraint
        await page.click('button:has-text("Add Inverter")');
        await page.waitForSelector('.inverter-constraint-card');
        await page.fill('.inverter-constraint-card .inv-name', 'solax-inverter-1');
        await page.fill('.inverter-constraint-card .inv-phase', '1');
        await page.fill('.inverter-constraint-card .inv-max-charge', '3000');
        await page.fill('.inverter-constraint-card .inv-max-discharge', '3000');
        await page.check('.inverter-constraint-card .inv-use-total');
        await page.check('.inverter-constraint-card .inv-grid-control');

        // 4. TOU Periods (Add a battery control period)
        console.log("Filling TOU Periods tab...");
        await page.click('.nav-btn:has-text("TOU Periods")');
        await page.click('#tab-periods button:has-text("Add Period")');
        await page.waitForSelector('.period-card');
        await page.fill('.period-card .period-name', 'PeakRate');
        await page.fill('.period-card .period-start', '14:00:00');
        await page.fill('.period-card .period-end', '20:00:00');
        await page.fill('.period-card .period-min-charge', '80');
        await page.fill('.period-card .period-force-discharge', '1200');
        await page.check('.period-card .period-grid-charge');
        await page.check('.period-card .period-grace');
        await page.check('.period-card .period-prefer-battery');

        // 5. Forwarders (EmonCMS, InfluxDB)
        console.log("Filling Forwarders tab...");
        await page.click('.nav-btn:has-text("Forwarders")');
        await page.check('#emon-enable');
        await page.fill('#emon-server', 'http://emoncms.local');
        await page.fill('#emon-timeout', '10');
        await page.fill('#emon-api', 'emon-api-key-xyz');

        await page.check('#influx-enable');
        await page.fill('#influx-url', 'http://influxdb.local:8086');
        await page.fill('#influx-db', 'power-scraper-db');
        await page.fill('#influx-measurement', 'power_telemetry');
        await page.fill('#influx-rp', '2w');
        await page.fill('#influx-user', 'influx-username');
        await page.fill('#influx-pass', 'influx-password');

        // 6. Electricity Tariff (Amber Electric)
        console.log("Filling Electricity Tariff tab...");
        await page.click('.nav-btn:has-text("Electricity Tariff")');
        await page.selectOption('#tariff-type', 'amber');
        await page.fill('#amber-api-key', 'amber-key-12345');
        await page.fill('#amber-site-id', 'amber-site-6789');
        await page.fill('#amber-api-url', 'https://api.amber.com.au');
        await page.check('#amber-neg-export-prevent');
        await page.check('#amber-low-price-charge');
        await page.fill('#amber-low-price-threshold', '5.5');
        await page.check('#amber-high-price-discharge');
        await page.fill('#amber-high-price-threshold', '65.2');

        // Fill Demand Settings
        await page.fill('#demand-start', '17:00');
        await page.fill('#demand-end', '21:00');
        await page.fill('#demand-rate', '0.155');

        // 7. Location (Latitude, Longitude, and PV Arrays)
        console.log("Filling Location tab...");
        await page.click('.nav-btn:has-text("Location")');
        await page.fill('#location-lat', '-33.8688');
        await page.fill('#location-lon', '151.2093');
        await page.click('button:has-text("Add PV Array")');
        await page.waitForSelector('.pv-array-card');
        await page.fill('.pv-array-card .array-name', 'East Roof');
        await page.fill('.pv-array-card .array-capacity', '4500');
        await page.fill('.pv-array-card .array-tilt', '22.5');
        await page.fill('.pv-array-card .array-azimuth', '-90');

        // Save and Apply Changes
        console.log("Applying complete valid configuration...");
        dialogText = null;
        await page.click('.btn-apply');
        
        let waitCountSave = 0;
        while (dialogText === null && waitCountSave < 50) {
            await page.waitForTimeout(100);
            waitCountSave++;
        }
        if (dialogText === null) {
            throw new Error("Timed out waiting for configuration save alert");
        }
        if (!dialogText.includes("Configuration updated and live reloaded successfully")) {
            throw new Error(`Unexpected save confirmation: ${dialogText}`);
        }
        console.log("Saved successfully!");

        // Reload page to verify saved values are correctly loaded from DB
        console.log("Reloading and verifying saved configuration...");
        await page.reload();
        await page.waitForSelector('#page-title');

        // Verify MQTT Values
        await page.click('.nav-btn:has-text("MQTT Settings")');
        if (await page.inputValue('#mqtt-broker') !== 'my-mqtt-broker.local' ||
            await page.inputValue('#mqtt-port') !== '1883' ||
            await page.inputValue('#mqtt-username') !== 'test-user' ||
            await page.inputValue('#mqtt-password') !== 'test-password' ||
            await page.inputValue('#mqtt-base') !== 'my-home-sensors' ||
            await page.isChecked('#mqtt-ha-discovery') !== true ||
            await page.inputValue('#mqtt-ha-prefix') !== 'homeassistant-custom') {
            throw new Error("MQTT configuration values did not reload correctly!");
        }

        // Verify Battery Control & Inverter Constraint
        await page.click('.nav-btn:has-text("Battery Control")');
        if (await page.isChecked('#battery-enable') !== true ||
            await page.inputValue('#battery-source') !== 'SolaX-Hybrid-Meter' ||
            await page.inputValue('#battery-tz') !== 'Australia/Sydney' ||
            await page.inputValue('#battery-grid-target') !== '150' ||
            await page.inputValue('#battery-init-mode') !== 'MpcArbitrage' ||
            await page.isChecked('#battery-linked') !== true) {
            throw new Error("Battery general config did not reload correctly!");
        }
        // Verify Inverter Constraint
        const cards = await page.$$('.inverter-constraint-card');
        let inverterFound = false;
        for (const card of cards) {
            const name = await card.$eval('.inv-name', el => el.value);
            if (name === 'solax-inverter-1') {
                inverterFound = true;
                const phase = await card.$eval('.inv-phase', el => el.value);
                const maxCharge = await card.$eval('.inv-max-charge', el => el.value);
                const maxDischarge = await card.$eval('.inv-max-discharge', el => el.value);
                const useTotal = await card.$eval('.inv-use-total', el => el.checked);
                const gridControl = await card.$eval('.inv-grid-control', el => el.checked);

                if (phase !== '1' || maxCharge !== '3000' || maxDischarge !== '3000' || useTotal !== true || gridControl !== true) {
                    throw new Error(`Inverter constraint values incorrect for solax-inverter-1: phase=${phase}, charge=${maxCharge}, discharge=${maxDischarge}, useTotal=${useTotal}, gridControl=${gridControl}`);
                }
                break;
            }
        }
        if (!inverterFound) {
            throw new Error("Could not find saved inverter constraint 'solax-inverter-1'");
        }

        // Verify TOU Period Card
        await page.click('.nav-btn:has-text("TOU Periods")');
        const periodCards = await page.$$('.period-card');
        let periodFound = false;
        for (const card of periodCards) {
            const name = await card.$eval('.period-name', el => el.value);
            if (name === 'PeakRate') {
                periodFound = true;
                const start = await card.$eval('.period-start', el => el.value);
                const end = await card.$eval('.period-end', el => el.value);
                const minCharge = await card.$eval('.period-min-charge', el => el.value);
                const forceDischarge = await card.$eval('.period-force-discharge', el => el.value);
                const gridCharge = await card.$eval('.period-grid-charge', el => el.checked);
                const grace = await card.$eval('.period-grace', el => el.checked);
                const preferBattery = await card.$eval('.period-prefer-battery', el => el.checked);

                if (start !== '14:00:00' || end !== '20:00:00' || minCharge !== '80' || forceDischarge !== '1200' || gridCharge !== true || grace !== true || preferBattery !== true) {
                    throw new Error(`TOU Period values incorrect for PeakRate: start=${start}, end=${end}, minCharge=${minCharge}, forceDischarge=${forceDischarge}, gridCharge=${gridCharge}, grace=${grace}, preferBattery=${preferBattery}`);
                }
                break;
            }
        }
        if (!periodFound) {
            throw new Error("Could not find saved TOU Period 'PeakRate'");
        }

        // Verify Forwarders
        await page.click('.nav-btn:has-text("Forwarders")');
        if (await page.isChecked('#emon-enable') !== true ||
            await page.inputValue('#emon-server') !== 'http://emoncms.local' ||
            await page.inputValue('#emon-timeout') !== '10' ||
            await page.inputValue('#emon-api') !== 'emon-api-key-xyz' ||
            await page.isChecked('#influx-enable') !== true ||
            await page.inputValue('#influx-url') !== 'http://influxdb.local:8086' ||
            await page.inputValue('#influx-db') !== 'power-scraper-db' ||
            await page.inputValue('#influx-measurement') !== 'power_telemetry' ||
            await page.inputValue('#influx-rp') !== '2w' ||
            await page.inputValue('#influx-user') !== 'influx-username' ||
            await page.inputValue('#influx-pass') !== 'influx-password') {
            throw new Error("Forwarders configuration did not reload correctly!");
        }

        // Verify Electricity Tariff (Amber)
        await page.click('.nav-btn:has-text("Electricity Tariff")');
        if (await page.inputValue('#tariff-type') !== 'amber' ||
            await page.inputValue('#amber-api-key') !== 'amber-key-12345' ||
            await page.inputValue('#amber-site-id') !== 'amber-site-6789' ||
            await page.inputValue('#amber-api-url') !== 'https://api.amber.com.au' ||
            await page.isChecked('#amber-neg-export-prevent') !== true ||
            await page.isChecked('#amber-low-price-charge') !== true ||
            await page.inputValue('#amber-low-price-threshold') !== '5.5' ||
            await page.isChecked('#amber-high-price-discharge') !== true ||
            await page.inputValue('#amber-high-price-threshold') !== '65.2' ||
            await page.inputValue('#demand-start') !== '17:00' ||
            await page.inputValue('#demand-end') !== '21:00' ||
            await page.inputValue('#demand-rate') !== '0.155') {
            throw new Error("Tariff configuration did not reload correctly!");
        }

        // Verify Location Settings
        await page.click('.nav-btn:has-text("Location")');
        if (await page.inputValue('#location-lat') !== '-33.8688' ||
            await page.inputValue('#location-lon') !== '151.2093') {
            throw new Error("Location coordinates did not reload correctly!");
        }
        const arrayCards = await page.$$('.pv-array-card');
        let arrayFound = false;
        for (const card of arrayCards) {
            const name = await card.$eval('.array-name', el => el.value);
            if (name === 'East Roof') {
                arrayFound = true;
                const capacity = await card.$eval('.array-capacity', el => el.value);
                const tilt = await card.$eval('.array-tilt', el => el.value);
                const azimuth = await card.$eval('.array-azimuth', el => el.value);
                if (capacity !== '4500' || tilt !== '22.5' || azimuth !== '-90') {
                    throw new Error(`PV Array values incorrect for East Roof: capacity=${capacity}, tilt=${tilt}, azimuth=${azimuth}`);
                }
                break;
            }
        }
        if (!arrayFound) {
            throw new Error("Could not find saved PV Array 'East Roof'");
        }

        console.log("Test 8 Passed successfully!");


        // Test Case 9: Edge Cases & Malformed Data Rejection
        console.log("Running Test 9: Edge Cases & Malformed Data Rejection...");

        // 1. Direct POST with type-mismatched data (timeout as string)
        console.log("1. Testing type-mismatched field value...");
        const currentConfig = await page.evaluate(() => currentConfig);
        const badPayload1 = JSON.parse(JSON.stringify(currentConfig));
        badPayload1.emoncms = {
            server: "http://emoncms.local",
            api_key: "my-key",
            timeout: "not-a-number" // should be f64
        };
        let response1 = await page.evaluate(async (data) => {
            const res = await fetch('/api/config', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(data)
            });
            return { status: res.status, text: await res.text() };
        }, badPayload1);

        if (response1.status !== 400 && response1.status !== 422) {
            throw new Error(`Expected status code 400 or 422 for type mismatch, got ${response1.status}. Response: ${response1.text}`);
        }
        console.log("Type-mismatched data was successfully rejected by the backend!");

        // 2. Direct POST with invalid JSON syntax
        console.log("2. Testing invalid JSON syntax...");
        let response2 = await page.evaluate(async () => {
            const res = await fetch('/api/config', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: '{"MQTT": {"broker": "localhost", "port": 1883' // missing closing braces
            });
            return { status: res.status, text: await res.text() };
        });

        if (response2.status !== 400 && response2.status !== 422) {
            throw new Error(`Expected status code 400 or 422 for invalid JSON syntax, got ${response2.status}`);
        }
        console.log("Invalid JSON syntax was successfully rejected by the backend!");

        // 3. Direct POST with missing required fields in sub-struct
        console.log("3. Testing missing required fields...");
        // MQTT struct requires 'broker' string (it is a mandatory field, broker: String)
        const badPayload3 = JSON.parse(JSON.stringify(currentConfig));
        delete badPayload3.MQTT.broker;
        let response3 = await page.evaluate(async (data) => {
            const res = await fetch('/api/config', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(data)
            });
            return { status: res.status, text: await res.text() };
        }, badPayload3);

        if (response3.status !== 400 && response3.status !== 422) {
            throw new Error(`Expected status code 400 or 422 for missing required field, got ${response3.status}. Response: ${response3.text}`);
        }
        console.log("Missing required field was successfully rejected by the backend!");

        console.log("Test 9 Passed successfully!");

        // Test Case 10: MQTT Connection Status & Connection Test
        console.log("Running Test 10: MQTT Connection Status & Connection Test...");

        // 1. Verify dashboard shows Connected (since we seeded it in web_ui_test_server)
        console.log("1. Checking MQTT connection state on dashboard...");
        // Wait for dashboard tab to be active
        await page.click('button:has-text("Dashboard")');
        const mqttStatusText = await page.textContent('#stat-mqtt-status');
        console.log(`MQTT Status on dashboard: "${mqttStatusText}"`);
        if (mqttStatusText.trim() !== "Connected") {
            throw new Error(`Expected MQTT status on dashboard to be "Connected", got "${mqttStatusText}"`);
        }
        
        // 2. Go to MQTT Settings tab
        console.log("2. Navigating to MQTT Settings tab...");
        await page.click('button:has-text("MQTT Settings")');

        // 3. Fill in invalid broker details to test failure reporting
        console.log("3. Testing connection test with invalid broker hostname...");
        await page.fill('#mqtt-broker', 'invalid.broker.local');
        await page.fill('#mqtt-port', '1883');
        await page.fill('#mqtt-username', '');
        await page.fill('#mqtt-password', '');
        await page.fill('#mqtt-base', 'sensors');
        
        // Click the test button
        console.log("Clicking 'Test MQTT Connection' button...");
        await page.click('#btn-mqtt-test');

        // Wait for the result text to change from the loading state and contain "Failed"
        console.log("Waiting for test connection result...");
        await page.waitForFunction(() => {
            const el = document.getElementById('mqtt-test-result');
            return el && (el.textContent.includes('Failed') || el.textContent.includes('Error') || el.textContent.includes('test failed'));
        }, { timeout: 10000 });

        const resultText = await page.textContent('#mqtt-test-result');
        console.log(`Test connection result: "${resultText}"`);
        if (!resultText.toLowerCase().includes('failed') && !resultText.toLowerCase().includes('error')) {
            throw new Error(`Expected test connection to report failure, but got: "${resultText}"`);
        }

        console.log("Test 10 Passed successfully!");


        console.log("\nAll Web UI Integration Tests PASSED successfully!");
        await browser.close();
        cleanup();
        process.exit(0);

    } catch (err) {
        console.error("\nIntegration Test FAILED:");
        console.error(err);
        if (browser) {
            await browser.close();
        }
        cleanup();
        process.exit(1);
    }
}

main();
