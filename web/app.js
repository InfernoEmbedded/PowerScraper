let currentConfig = {};

// Tab Switching
function switchTab(tabId, el) {
    document.querySelectorAll('.tab-content').forEach(t => t.classList.remove('active'));
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    document.getElementById(tabId).classList.add('active');
    el.classList.add('active');

    const pageTitle = el.innerText.trim();
    document.getElementById('page-title').innerText = pageTitle === "Dashboard" ? "System Dashboard" : pageTitle;
}

function toggleFormSection(sectionId, enabled) {
    const section = document.getElementById(sectionId);
    if (section) {
        const inputs = section.querySelectorAll('input, select, textarea, button');
        inputs.forEach(input => input.disabled = !enabled);
        section.style.opacity = enabled ? '1' : '0.4';
    }
}

// Fetch telemetry and status updates
async function fetchStatus() {
    try {
        const r = await fetch('/api/status');
        if (!r.ok) return;
        const status = await r.json();

        document.getElementById('stat-mains').innerText = `${status.meter_power.toFixed(0)} W`;
        document.getElementById('stat-mode').innerText = status.active_mode || "Auto";
        document.getElementById('stat-target').innerText = `${status.grid_target.toFixed(0)} W`;

        // Update instantaneous control UI active states
        document.querySelectorAll('.mode-card').forEach(c => c.classList.remove('active'));
        if (status.active_mode === "Auto") document.getElementById('mode-auto').classList.add('active');
        if (status.active_mode === "ChargeBatteries") document.getElementById('mode-charge').classList.add('active');
        if (status.active_mode === "MaximumFeedin") document.getElementById('mode-feedin').classList.add('active');

        // Render inverter lists
        const list = document.getElementById('dash-inverters-list');
        const keys = Object.keys(status.inverters);
        if (keys.length === 0) {
            list.innerHTML = `<div class="inverter-item" style="color: var(--text-muted); text-align: center; grid-template-columns: 1fr;">No inverters connected.</div>`;
        } else {
            list.innerHTML = keys.map(k => {
                const inv = status.inverters[k];
                return `
                    <div class="inverter-item">
                        <div>
                            <div class="inverter-field-title">Inverter ID</div>
                            <div class="inverter-field-val">${k}</div>
                        </div>
                        <div>
                            <div class="inverter-field-title">Battery Capacity (SOC)</div>
                            <div class="inverter-field-val">${inv.battery_capacity} %</div>
                        </div>
                        <div>
                            <div class="inverter-field-title">Charge/Discharge Power</div>
                            <div class="inverter-field-val">${inv.battery_power} W</div>
                        </div>
                        <div>
                            <div class="inverter-field-title">PV Output Power</div>
                            <div class="inverter-field-val">${inv.pv_power} W</div>
                        </div>
                    </div>
                `;
            }).join('');
        }
    } catch (e) {
        console.error("Failed to fetch live stats", e);
    }
}

// Live Target slider updates
function updateTargetText(val) {
    document.getElementById('instant-target-slider').value = val;
    document.getElementById('instant-target-val').value = val;
}

async function setInstantMode(mode) {
    try {
        // To apply instant mode, we modify the active config's initial-mode and save
        const newCfg = JSON.parse(JSON.stringify(currentConfig));
        if (!newCfg.BatteryControl) newCfg.BatteryControl = {};
        newCfg.BatteryControl["initial-mode"] = mode;
        
        const resp = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(newCfg)
        });
        if (resp.ok) {
            currentConfig = newCfg;
            fetchStatus();
        }
    } catch(e) {
        console.error("Failed to change mode", e);
    }
}

async function applyInstantTarget() {
    const targetVal = parseFloat(document.getElementById('instant-target-val').value);
    if (isNaN(targetVal)) return;
    try {
        const newCfg = JSON.parse(JSON.stringify(currentConfig));
        if (!newCfg.BatteryControl) newCfg.BatteryControl = {};
        newCfg.BatteryControl["grid-target"] = targetVal;
        
        const resp = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(newCfg)
        });
        if (resp.ok) {
            currentConfig = newCfg;
            fetchStatus();
        }
    } catch(e) {
        console.error("Failed to set target", e);
    }
}

// Loading configuration to Forms
async function loadConfig() {
    try {
        const r = await fetch('/api/config');
        if (!r.ok) return;
        const config = await r.json();
        currentConfig = config;

        // Load MQTT
        const mqtt = config.MQTT || {};
        document.getElementById('mqtt-broker').value = mqtt.broker || '';
        document.getElementById('mqtt-port').value = mqtt.port || '';
        document.getElementById('mqtt-username').value = mqtt.username || '';
        document.getElementById('mqtt-password').value = mqtt.password || '';
        document.getElementById('mqtt-base').value = mqtt["base-topic"] || '';
        document.getElementById('mqtt-ha-discovery').checked = mqtt["home-assistant-discovery"] !== false;
        document.getElementById('mqtt-ha-prefix').value = mqtt["home-assistant-prefix"] || '';

        // Load WiFi
        const wifi = config["Solax-Wifi"];
        document.getElementById('wifi-enable').checked = !!wifi;
        toggleFormSection('wifi-section', !!wifi);
        if (wifi) {
            document.getElementById('wifi-poll').value = wifi["poll-period"] || 10;
            document.getElementById('wifi-timeout').value = wifi.timeout || 5;
            document.getElementById('wifi-inverters').value = (wifi.inverters || []).join(', ');
        }

        // Load Modbus Standard
        const modbus = config["Solax-Modbus"];
        document.getElementById('modbus-enable').checked = !!modbus;
        toggleFormSection('modbus-section', !!modbus);
        if (modbus) {
            document.getElementById('modbus-poll').value = modbus["poll-period"] || 10;
            document.getElementById('modbus-timeout').value = modbus.timeout || 5;
            document.getElementById('modbus-pwd').value = modbus["installer-password"] || '';
            document.getElementById('modbus-avg').value = modbus["power-budget-avg-samples"] || 30;
            document.getElementById('modbus-inverters').value = (modbus.inverters || []).join(', ');
            document.getElementById('modbus-hostnames').value = (modbus.hostnames || []).join(', ');
        }

        // Load XHybrid Modbus
        const hybrid = config["Solax-XHybrid-Modbus"];
        document.getElementById('hybrid-enable').checked = !!hybrid;
        toggleFormSection('hybrid-section', !!hybrid);
        if (hybrid) {
            document.getElementById('hybrid-poll').value = hybrid["poll-period"] || 10;
            document.getElementById('hybrid-timeout').value = hybrid.timeout || 5;
            document.getElementById('hybrid-pwd').value = hybrid["installer-password"] || '';
            document.getElementById('hybrid-avg').value = hybrid["power-budget-avg-samples"] || 30;
            document.getElementById('hybrid-inverters').value = (hybrid.inverters || []).join(', ');
            document.getElementById('hybrid-hostnames').value = (hybrid.hostnames || []).join(', ');
        }

        // Load SDM630
        const sdm = config.SDM630Modbusv2;
        document.getElementById('sdm-enable').checked = !!sdm;
        toggleFormSection('sdm-section', !!sdm);
        if (sdm) {
            document.getElementById('sdm-poll').value = sdm["poll-period"] || 1;
            document.getElementById('sdm-timeout').value = sdm.timeout || 1;
            document.getElementById('sdm-baud').value = sdm.baud || 38400;
            document.getElementById('sdm-parity').value = sdm.parity || 'E';
            document.getElementById('sdm-stop').value = sdm.stopbits || 1;
            document.getElementById('sdm-ports').value = (sdm.ports || []).join(', ');
        }

        // Load DTSU666
        const dtsu = config.DTSU666;
        document.getElementById('dtsu-enable').checked = !!dtsu;
        toggleFormSection('dtsu-section', !!dtsu);
        if (dtsu) {
            document.getElementById('dtsu-poll').value = dtsu["poll-period"] || 1;
            document.getElementById('dtsu-timeout').value = dtsu.timeout || 1;
            document.getElementById('dtsu-baud').value = dtsu.baud || 9600;
            document.getElementById('dtsu-parity').value = dtsu.parity || 'N';
            document.getElementById('dtsu-stop').value = dtsu.stopbits || 1;
            document.getElementById('dtsu-ports').value = (dtsu.ports || []).join(', ');
        }

        // Load MQTT Power Meter
        const mqMeter = config.MQTTPowerMeter;
        document.getElementById('mqtt-meter-enable').checked = !!mqMeter;
        toggleFormSection('mqtt-meter-section', !!mqMeter);
        if (mqMeter && mqMeter.meters && mqMeter.meters.length > 0) {
            const meterName = mqMeter.meters[0];
            const mDev = mqMeter.meter_devices[meterName] || {};
            document.getElementById('mqtt-meter-poll').value = mqMeter["poll-period"] || 10;
            document.getElementById('mqtt-meter-name').value = meterName || '';
            document.getElementById('mqtt-meter-broker').value = mDev.broker || '';
            document.getElementById('mqtt-meter-port').value = mDev.port || '';
            document.getElementById('mqtt-meter-user').value = mDev.username || '';
            document.getElementById('mqtt-meter-pass').value = mDev.password || '';
            document.getElementById('mqtt-meter-topic-total').value = mDev.topic_total || '';
            document.getElementById('mqtt-meter-topic-p1').value = mDev.topic_phase1 || '';
            document.getElementById('mqtt-meter-topic-p2').value = mDev.topic_phase2 || '';
            document.getElementById('mqtt-meter-topic-p3').value = mDev.topic_phase3 || '';
        }

        // Load Battery Control Config
        const bat = config["Solax-BatteryControl"];
        document.getElementById('battery-enable').checked = !!bat;
        toggleFormSection('battery-section', !!bat);
        if (bat) {
            document.getElementById('battery-source').value = bat.source || '';
            document.getElementById('battery-tz').value = bat.timezone || 'UTC';
            document.getElementById('battery-grid-target').value = bat["grid-target"] || 0.0;
            document.getElementById('battery-init-mode').value = bat["initial-mode"] || 'Auto';
            document.getElementById('battery-linked').checked = bat["linked-batteries"] === true;

            // Load Instant controls defaults
            document.getElementById('instant-target-val').value = bat["grid-target"] || 0.0;
            document.getElementById('instant-target-slider').value = bat["grid-target"] || 0.0;

            // Render inverter constraints list
            const listDiv = document.getElementById('inverters-constraints-list');
            listDiv.innerHTML = '';
            if (bat.inverter) {
                Object.keys(bat.inverter).forEach(name => {
                    const inv = bat.inverter[name];
                    renderInverterConstraintCard(name, inv);
                });
            }
        }

        // Load TOU Periods
        const periodsContainer = document.getElementById('periods-list-container');
        periodsContainer.innerHTML = '';
        if (bat && bat.period) {
            Object.keys(bat.period).forEach(pName => {
                const per = bat.period[pName];
                renderPeriodCard(pName, per);
            });
        }

        // Load EmonCMS
        const emon = config.emoncms;
        document.getElementById('emon-enable').checked = !!emon;
        toggleFormSection('emon-section', !!emon);
        if (emon) {
            document.getElementById('emon-server').value = emon.server || '';
            document.getElementById('emon-timeout').value = emon.timeout || 5;
            document.getElementById('emon-api').value = emon.api_key || '';
        }

        // Load InfluxDB
        const influx = config.influx;
        document.getElementById('influx-enable').checked = !!influx;
        toggleFormSection('influx-section', !!influx);
        if (influx) {
            document.getElementById('influx-url').value = influx.influx_url || '';
            document.getElementById('influx-db').value = influx.influx_database || '';
            document.getElementById('influx-measurement').value = influx.influx_measurement || '';
            document.getElementById('influx-rp').value = influx.influx_retention_policy || 'autogen';
            document.getElementById('influx-user').value = influx.influx_user || '';
            document.getElementById('influx-pass').value = influx.influx_pass || '';
        }
    } catch (e) {
        console.error("Failed to load config backend", e);
    }
}

// Dynamic Card Rendering helpers
function renderInverterConstraintCard(name, inv) {
    const listDiv = document.getElementById('inverters-constraints-list');
    const card = document.createElement('div');
    card.className = 'list-item-card inverter-constraint-card';
    card.innerHTML = `
        <div style="flex: 1; display: flex; flex-direction: column; gap: 10px;">
            <div class="form-row">
                <div class="form-group">
                    <label>Inverter Name</label>
                    <input type="text" class="inv-name" value="${name}">
                </div>
                <div class="form-group">
                    <label>Wiring Phase</label>
                    <input type="number" class="inv-phase" value="${inv.phase || 1}">
                </div>
                <div class="form-group">
                    <label>Max Charge Rate (W)</label>
                    <input type="number" class="inv-max-charge" value="${inv["max-charge"] || 2000}">
                </div>
                <div class="form-group">
                    <label>Max Discharge Rate (W)</label>
                    <input type="number" class="inv-max-discharge" value="${inv["max-discharge"] || 2000}">
                </div>
            </div>
            <div class="form-row">
                <div class="checkbox-group">
                    <input type="checkbox" class="inv-use-total" ${inv["use-total-power"] ? 'checked' : ''}>
                    <label>Regulate Total Grid Power</label>
                </div>
                <div class="checkbox-group">
                    <input type="checkbox" class="inv-grid-control" ${inv["control-grid-power"] ? 'checked' : ''}>
                    <label>Grid Power Control mode</label>
                </div>
            </div>
        </div>
        <button class="sub-btn danger" style="margin-left: 20px;" onclick="this.parentElement.remove()">Remove</button>
    `;
    listDiv.appendChild(card);
}

function addInverterConstraint() {
    renderInverterConstraintCard('new-inverter', {
        phase: 1,
        "max-charge": 2000,
        "max-discharge": 2000,
        "use-total-power": false,
        "control-grid-power": false
    });
}

function renderPeriodCard(pName, per) {
    const container = document.getElementById('periods-list-container');
    const card = document.createElement('div');
    card.className = 'list-item-card period-card';
    card.innerHTML = `
        <div style="flex: 1; display: flex; flex-direction: column; gap: 10px;">
            <div class="form-row">
                <div class="form-group">
                    <label>Period Identifier</label>
                    <input type="text" class="period-name" value="${pName}">
                </div>
                <div class="form-group">
                    <label>Start Time</label>
                    <input type="text" class="period-start" value="${per.start || '00:00:00'}">
                </div>
                <div class="form-group">
                    <label>End Time</label>
                    <input type="text" class="period-end" value="${per.end || '23:59:59'}">
                </div>
                <div class="form-group">
                    <label>Min SOC (%)</label>
                    <input type="number" class="period-min-charge" value="${per["min-charge"] || 20}">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Force Discharge Rate (W)</label>
                    <input type="number" class="period-force-discharge" value="${per["force-discharge"] || ''}" placeholder="None">
                </div>
                <div class="checkbox-group">
                    <input type="checkbox" class="period-grid-charge" ${per["grid-charge"] ? 'checked' : ''}>
                    <label>Allow Charging from Grid</label>
                </div>
                <div class="checkbox-group">
                    <input type="checkbox" class="period-grace" ${per.grace ? 'checked' : ''}>
                    <label>Enable Grace Capacity early stops</label>
                </div>
                <div class="checkbox-group">
                    <input type="checkbox" class="period-prefer-battery" ${per["prefer-battery"] ? 'checked' : ''}>
                    <label>Prioritize Battery Charge</label>
                </div>
            </div>
        </div>
        <button class="sub-btn danger" style="margin-left: 20px;" onclick="this.parentElement.remove()">Remove</button>
    `;
    container.appendChild(card);
}

function addTOUPeriod() {
    renderPeriodCard('NewPeriod', {
        start: '00:00:00',
        end: '23:59:59',
        "min-charge": 20,
        "grid-charge": false,
        grace: false,
        "prefer-battery": false
    });
}

// Parsing comma lists
function parseCommaList(val) {
    if (!val.trim()) return [];
    return val.split(',').map(s => s.trim()).filter(s => s.length > 0);
}

// Saving config JSON
async function saveConfiguration() {
    const cfg = {
        MQTT: {
            broker: document.getElementById('mqtt-broker').value,
            port: parseInt(document.getElementById('mqtt-port').value) || 1883,
            "base-topic": document.getElementById('mqtt-base').value || "sensors",
            username: document.getElementById('mqtt-username').value || null,
            password: document.getElementById('mqtt-password').value || null,
            "home-assistant-discovery": document.getElementById('mqtt-ha-discovery').checked,
            "home-assistant-prefix": document.getElementById('mqtt-ha-prefix').value || "homeassistant"
        }
    };

    // Wifi Inverter
    if (document.getElementById('wifi-enable').checked) {
        cfg["Solax-Wifi"] = {
            "poll-period": parseInt(document.getElementById('wifi-poll').value) || 10,
            timeout: parseInt(document.getElementById('wifi-timeout').value) || 5,
            inverters: parseCommaList(document.getElementById('wifi-inverters').value)
        };
    } else {
        cfg["Solax-Wifi"] = null;
    }

    // Modbus Inverter
    if (document.getElementById('modbus-enable').checked) {
        cfg["Solax-Modbus"] = {
            "poll-period": parseInt(document.getElementById('modbus-poll').value) || 10,
            timeout: parseInt(document.getElementById('modbus-timeout').value) || 5,
            "installer-password": parseInt(document.getElementById('modbus-pwd').value) || null,
            "power-budget-avg-samples": parseInt(document.getElementById('modbus-avg').value) || 30,
            inverters: parseCommaList(document.getElementById('modbus-inverters').value),
            hostnames: parseCommaList(document.getElementById('modbus-hostnames').value)
        };
    } else {
        cfg["Solax-Modbus"] = null;
    }

    // XHybrid Inverter
    if (document.getElementById('hybrid-enable').checked) {
        cfg["Solax-XHybrid-Modbus"] = {
            "poll-period": parseInt(document.getElementById('hybrid-poll').value) || 10,
            timeout: parseInt(document.getElementById('hybrid-timeout').value) || 5,
            "installer-password": parseInt(document.getElementById('hybrid-pwd').value) || null,
            "power-budget-avg-samples": parseInt(document.getElementById('hybrid-avg').value) || 30,
            inverters: parseCommaList(document.getElementById('hybrid-inverters').value),
            hostnames: parseCommaList(document.getElementById('hybrid-hostnames').value)
        };
    } else {
        cfg["Solax-XHybrid-Modbus"] = null;
    }

    // SDM630 Meter
    if (document.getElementById('sdm-enable').checked) {
        cfg.SDM630Modbusv2 = {
            "poll-period": parseInt(document.getElementById('sdm-poll').value) || 1,
            timeout: parseInt(document.getElementById('sdm-timeout').value) || 1,
            baud: parseInt(document.getElementById('sdm-baud').value) || 38400,
            parity: document.getElementById('sdm-parity').value || 'E',
            stopbits: parseInt(document.getElementById('sdm-stop').value) || 1,
            ports: parseCommaList(document.getElementById('sdm-ports').value)
        };
    } else {
        cfg.SDM630Modbusv2 = null;
    }

    // DTSU666 Meter
    if (document.getElementById('dtsu-enable').checked) {
        cfg.DTSU666 = {
            "poll-period": parseInt(document.getElementById('dtsu-poll').value) || 1,
            timeout: parseInt(document.getElementById('dtsu-timeout').value) || 1,
            baud: parseInt(document.getElementById('dtsu-baud').value) || 9600,
            parity: document.getElementById('dtsu-parity').value || 'N',
            stopbits: parseInt(document.getElementById('dtsu-stop').value) || 1,
            ports: parseCommaList(document.getElementById('dtsu-ports').value)
        };
    } else {
        cfg.DTSU666 = null;
    }

    // MQTT Custom Meter
    if (document.getElementById('mqtt-meter-enable').checked) {
        const mName = document.getElementById('mqtt-meter-name').value || "MainsMeter";
        cfg.MQTTPowerMeter = {
            "poll-period": parseInt(document.getElementById('mqtt-meter-poll').value) || 10,
            meters: [mName],
            meter_devices: {}
        };
        cfg.MQTTPowerMeter.meter_devices[mName] = {
            broker: document.getElementById('mqtt-meter-broker').value,
            port: parseInt(document.getElementById('mqtt-meter-port').value) || 1883,
            username: document.getElementById('mqtt-meter-user').value || null,
            password: document.getElementById('mqtt-meter-pass').value || null,
            topic_total: document.getElementById('mqtt-meter-topic-total').value || null,
            topic_phase1: document.getElementById('mqtt-meter-topic-p1').value || null,
            topic_phase2: document.getElementById('mqtt-meter-topic-p2').value || null,
            topic_phase3: document.getElementById('mqtt-meter-topic-p3').value || null
        };
    } else {
        cfg.MQTTPowerMeter = null;
    }

    // Battery Control
    if (document.getElementById('battery-enable').checked) {
        cfg["Solax-BatteryControl"] = {
            source: document.getElementById('battery-source').value || null,
            timezone: document.getElementById('battery-tz').value || "UTC",
            "grid-target": parseFloat(document.getElementById('battery-grid-target').value) || 0.0,
            "initial-mode": document.getElementById('battery-init-mode').value || "Auto",
            "linked-batteries": document.getElementById('battery-linked').checked,
            inverter: {},
            period: {}
        };

        // Compile inverters
        document.querySelectorAll('.inverter-constraint-card').forEach(card => {
            const name = card.querySelector('.inv-name').value.trim();
            if (name) {
                cfg["Solax-BatteryControl"].inverter[name] = {
                    phase: parseInt(card.querySelector('.inv-phase').value) || 1,
                    "max-charge": parseFloat(card.querySelector('.inv-max-charge').value) || 2000.0,
                    "max-discharge": parseFloat(card.querySelector('.inv-max-discharge').value) || 2000.0,
                    "use-total-power": card.querySelector('.inv-use-total').checked,
                    "control-grid-power": card.querySelector('.inv-grid-control').checked
                };
            }
        });

        // Compile periods
        document.querySelectorAll('.period-card').forEach(card => {
            const pName = card.querySelector('.period-name').value.trim();
            const forceVal = parseFloat(card.querySelector('.period-force-discharge').value);
            if (pName) {
                cfg["Solax-BatteryControl"].period[pName] = {
                    start: card.querySelector('.period-start').value.trim() || "00:00:00",
                    end: card.querySelector('.period-end').value.trim() || "23:59:59",
                    "min-charge": parseInt(card.querySelector('.period-min-charge').value) || 20,
                    "grid-charge": card.querySelector('.period-grid-charge').checked,
                    "force-discharge": isNaN(forceVal) ? null : forceVal,
                    grace: card.querySelector('.period-grace').checked,
                    "prefer-battery": card.querySelector('.period-prefer-battery').checked
                };
            }
        });
    } else {
        cfg["Solax-BatteryControl"] = null;
    }

    // EmonCMS
    if (document.getElementById('emon-enable').checked) {
        cfg.emoncms = {
            server: document.getElementById('emon-server').value,
            timeout: parseInt(document.getElementById('emon-timeout').value) || 5,
            api_key: document.getElementById('emon-api').value
        };
    } else {
        cfg.emoncms = null;
    }

    // InfluxDB
    if (document.getElementById('influx-enable').checked) {
        cfg.influx = {
            influx_url: document.getElementById('influx-url').value,
            influx_database: document.getElementById('influx-db').value,
            influx_measurement: document.getElementById('influx-measurement').value,
            influx_retention_policy: document.getElementById('influx-rp').value || "autogen",
            influx_user: document.getElementById('influx-user').value,
            influx_pass: document.getElementById('influx-pass').value
        };
    } else {
        cfg.influx = null;
    }

    try {
        const resp = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(cfg)
        });
        if (resp.ok) {
            alert("Configuration updated and live reloaded successfully!");
            currentConfig = cfg;
            loadConfig();
        } else {
            const err = await resp.text();
            alert(`Failed to apply configuration: ${err}`);
        }
    } catch (e) {
        alert(`Network error saving configuration: ${e}`);
    }
}

// Initialize UI
window.addEventListener('load', () => {
    loadConfig();
    fetchStatus();
    setInterval(fetchStatus, 3000); // Poll status every 3s
});
