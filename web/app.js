let currentConfig = {};
let lastStatusData = null;

// Tab Switching
function switchTab(tabId, el) {
    document.querySelectorAll('.tab-content').forEach(t => t.classList.remove('active'));
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    document.getElementById(tabId).classList.add('active');
    el.classList.add('active');

    const pageTitle = el.innerText.trim();
    document.getElementById('page-title').innerText = pageTitle === "Dashboard" ? "System Dashboard" : pageTitle;

    // Show/hide 'Apply Changes' button based on tabId
    const applyBtn = document.querySelector('.btn-apply');
    if (applyBtn) {
        if (tabId === 'tab-dashboard' || tabId === 'tab-simulation' || tabId === 'tab-about') {
            applyBtn.style.display = 'none';
        } else {
            applyBtn.style.display = 'inline-block';
        }
    }
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
        lastStatusData = status;

        // Style and update Current Grid Power
        const mainsEl = document.getElementById('stat-mains');
        const mainsPower = status.meter_power || 0.0;
        const absMainsPower = Math.abs(mainsPower);
        if (mainsPower > 0) {
            mainsEl.innerText = `${absMainsPower.toFixed(0)} W Draw`;
            mainsEl.style.color = 'var(--danger)';
            mainsEl.style.textShadow = '0 0 10px rgba(239, 68, 68, 0.2)';
        } else {
            mainsEl.innerText = `${absMainsPower.toFixed(0)} W Feedin`;
            mainsEl.style.color = 'var(--accent)';
            mainsEl.style.textShadow = '0 0 10px var(--accent-glow)';
        }

        const mainsUpdatedEl = document.getElementById('stat-mains-updated');
        if (mainsUpdatedEl) {
            if (status.meter_last_updated) {
                mainsUpdatedEl.setAttribute('data-timestamp', status.meter_last_updated);
            } else {
                mainsUpdatedEl.removeAttribute('data-timestamp');
                mainsUpdatedEl.innerText = '';
            }
        }

        document.getElementById('stat-mode').innerText = status.active_mode || "Auto";

        if (status.version) {
            const versionEl = document.getElementById('about-version');
            if (versionEl) {
                versionEl.innerText = status.version;
            }
        }

        // Style and update Grid Target card
        const targetEl = document.getElementById('stat-target');
        const targetPower = status.grid_target || 0.0;
        const absTargetPower = Math.abs(targetPower);
        if (targetPower > 0) {
            targetEl.innerText = `${absTargetPower.toFixed(0)} W Draw`;
            targetEl.style.color = 'var(--danger)';
            targetEl.style.textShadow = '0 0 10px rgba(239, 68, 68, 0.2)';
        } else {
            targetEl.innerText = `${absTargetPower.toFixed(0)} W Feedin`;
            targetEl.style.color = 'var(--accent)';
            targetEl.style.textShadow = '0 0 10px var(--accent-glow)';
        }

        // Update instantaneous control UI active states
        document.querySelectorAll('.mode-card').forEach(c => c.classList.remove('active'));
        if (status.active_mode === "Auto") document.getElementById('mode-auto').classList.add('active');
        if (status.active_mode === "ChargeBatteries") document.getElementById('mode-charge').classList.add('active');
        if (status.active_mode === "MaximumFeedin") document.getElementById('mode-feedin').classList.add('active');
        if (status.active_mode === "SmartHeuristic") document.getElementById('mode-smart').classList.add('active');
        if (status.active_mode === "AdaptivePeakShaving") document.getElementById('mode-adaptive').classList.add('active');
        if (status.active_mode === "MpcOptimizer") document.getElementById('mode-mpc').classList.add('active');

        // Render inverter lists and calculate total inverter interaction and total solar power
        const list = document.getElementById('dash-inverters-list');
        const keys = Object.keys(status.inverters).sort((a, b) => a.localeCompare(b, undefined, {numeric: true, sensitivity: 'base'}));
        
        let totalInvBatteryPower = 0.0;
        let totalSolarPower = 0.0;
        
        if (keys.length === 0) {
            list.innerHTML = `<div class="inverter-item" style="color: var(--text-muted); text-align: center; grid-template-columns: 1fr;">No inverters connected.</div>`;
        } else {
            list.innerHTML = keys.map(k => {
                const inv = status.inverters[k];
                totalInvBatteryPower += (inv.battery_power || 0.0);
                totalSolarPower += (inv.pv_power || 0.0);

                let batPowerStr = "";
                let batPowerStyle = "";
                const val = inv.battery_power || 0.0;
                const absVal = Math.abs(val);
                if (val < 0) {
                    batPowerStr = `${absVal.toFixed(0)} W Charge`;
                    batPowerStyle = `color: var(--accent); text-shadow: 0 0 8px var(--accent-glow);`;
                } else if (val > 0) {
                    batPowerStr = `${absVal.toFixed(0)} W Discharge`;
                    batPowerStyle = `color: var(--danger); text-shadow: 0 0 8px rgba(239, 68, 68, 0.25);`;
                } else {
                    batPowerStr = `0 W Idle`;
                    batPowerStyle = `color: var(--text-muted);`;
                }

                return `
                    <div class="inverter-item">
                        <div>
                            <div class="inverter-field-title">Inverter ID</div>
                            <div class="inverter-field-val">${k}</div>
                            <div class="stat-updated inverter-age" data-timestamp="${inv.last_updated || ''}"></div>
                        </div>
                        <div>
                            <div class="inverter-field-title">Battery Capacity (SOC)</div>
                            <div class="inverter-field-val">${inv.battery_capacity} %</div>
                        </div>
                        <div>
                            <div class="inverter-field-title">Charge/Discharge Power</div>
                            <div class="inverter-field-val" style="${batPowerStyle}">${batPowerStr}</div>
                        </div>
                        <div>
                            <div class="inverter-field-title">PV Output Power</div>
                            <div class="inverter-field-val">${inv.pv_power} W</div>
                        </div>
                    </div>
                `;
            }).join('');
        }

        // Style and update Total Inverter Grid Interaction card
        const totalInvEl = document.getElementById('stat-total-inverter');
        const absTotalInv = Math.abs(totalInvBatteryPower);
        if (totalInvBatteryPower > 0) {
            // Battery is discharging -> feeding power to the home/grid
            totalInvEl.innerText = `${absTotalInv.toFixed(0)} W Feedin`;
            totalInvEl.style.color = 'var(--accent)';
            totalInvEl.style.textShadow = '0 0 10px var(--accent-glow)';
        } else if (totalInvBatteryPower < 0) {
            // Battery is charging -> drawing power from the home/grid
            totalInvEl.innerText = `${absTotalInv.toFixed(0)} W Draw`;
            totalInvEl.style.color = 'var(--danger)';
            totalInvEl.style.textShadow = '0 0 10px rgba(239, 68, 68, 0.2)';
        } else {
            totalInvEl.innerText = `0 W Idle`;
            totalInvEl.style.color = 'var(--text-muted)';
            totalInvEl.style.textShadow = 'none';
        }

        // Update Total Solar Power card
        const totalSolarEl = document.getElementById('stat-total-solar');
        if (totalSolarEl) {
            totalSolarEl.innerText = `${totalSolarPower.toFixed(0)} W`;
        }

        const mqttStatusEl = document.getElementById('stat-mqtt-status');
        if (mqttStatusEl) {
            if (status.mqtt_connected) {
                mqttStatusEl.innerText = "Connected";
                mqttStatusEl.style.color = "var(--accent)";
                mqttStatusEl.style.textShadow = "0 0 10px var(--accent-glow)";
            } else {
                mqttStatusEl.innerText = "Disconnected";
                mqttStatusEl.style.color = "var(--danger)";
                mqttStatusEl.style.textShadow = "0 0 10px rgba(239, 68, 68, 0.2)";
            }
        }

        const importPriceEl = document.getElementById('stat-import-price');
        if (importPriceEl) {
            if (status.import_price !== undefined && status.import_price !== null) {
                importPriceEl.innerText = `${status.import_price.toFixed(1)} c/kWh`;
                if (status.price_thresholds) {
                    const t = status.price_thresholds;
                    if (status.import_price >= t.import_70) {
                        importPriceEl.style.color = 'var(--danger)';
                        importPriceEl.style.textShadow = '0 0 10px hsla(350, 80%, 55%, 0.35)';
                    } else if (status.import_price <= t.import_30) {
                        importPriceEl.style.color = 'var(--accent)';
                        importPriceEl.style.textShadow = '0 0 10px var(--accent-glow)';
                    } else {
                        importPriceEl.style.color = 'var(--warning)';
                        importPriceEl.style.textShadow = '0 0 10px hsla(35, 90%, 55%, 0.25)';
                    }
                } else {
                    importPriceEl.style.color = 'var(--primary)';
                    importPriceEl.style.textShadow = '0 0 10px var(--primary-glow)';
                }
            } else {
                importPriceEl.innerText = `-- c/kWh`;
                importPriceEl.style.color = 'var(--primary)';
                importPriceEl.style.textShadow = '0 0 10px var(--primary-glow)';
            }
        }

        const exportPriceEl = document.getElementById('stat-export-price');
        if (exportPriceEl) {
            if (status.export_price !== undefined && status.export_price !== null) {
                const dispPrice = status.export_price * -1;
                exportPriceEl.innerText = `${dispPrice.toFixed(1)} c/kWh`;
                if (status.price_thresholds) {
                    const t = status.price_thresholds;
                    if (status.export_price >= t.export_70) {
                        exportPriceEl.style.color = 'var(--accent)';
                        exportPriceEl.style.textShadow = '0 0 10px var(--accent-glow)';
                    } else if (status.export_price <= t.export_30) {
                        exportPriceEl.style.color = 'var(--danger)';
                        exportPriceEl.style.textShadow = '0 0 10px hsla(350, 80%, 55%, 0.35)';
                    } else {
                        exportPriceEl.style.color = 'var(--warning)';
                        exportPriceEl.style.textShadow = '0 0 10px hsla(35, 90%, 55%, 0.25)';
                    }
                } else {
                    exportPriceEl.style.color = 'var(--primary)';
                    exportPriceEl.style.textShadow = '0 0 10px var(--primary-glow)';
                }
            } else {
                exportPriceEl.innerText = `-- c/kWh`;
                exportPriceEl.style.color = 'var(--primary)';
                exportPriceEl.style.textShadow = '0 0 10px var(--primary-glow)';
            }
        }

        // Update calculated battery capacity fields in constraint cards
        document.querySelectorAll('.inverter-constraint-card').forEach(card => {
            const nameEl = card.querySelector('.inv-name');
            if (nameEl) {
                const name = nameEl.value.trim();
                const invStatus = status.inverters && status.inverters[name];
                if (invStatus && invStatus.calculated_battery_capacity !== undefined && invStatus.calculated_battery_capacity !== null) {
                    const calcEl = card.querySelector('.inv-calc-capacity');
                    if (calcEl) {
                        calcEl.value = `${invStatus.calculated_battery_capacity.toFixed(2)} kWh`;
                    }
                }
            }
        });

        // Run the counters update immediately to prevent content layout shift / blank age counters
        updateAgeCounters();
    } catch (e) {
        console.error("Failed to fetch live stats", e);
    }
}

function colorScrollbox(val) {
    const num = parseFloat(val) || 0;
    const el = document.getElementById('instant-target-val');
    if (!el) return;
    if (num > 0) {
        el.style.color = 'var(--danger)';
        el.style.borderColor = 'rgba(239, 68, 68, 0.4)';
        el.style.boxShadow = '0 0 10px rgba(239, 68, 68, 0.15)';
    } else if (num < 0) {
        el.style.color = 'var(--accent)';
        el.style.borderColor = 'rgba(16, 185, 129, 0.4)';
        el.style.boxShadow = '0 0 10px rgba(16, 185, 129, 0.15)';
    } else {
        el.style.color = 'var(--text-main)';
        el.style.borderColor = 'var(--border-color)';
        el.style.boxShadow = 'none';
    }
}

// Live Target slider updates
function updateTargetText(val) {
    document.getElementById('instant-target-slider').value = val;
    document.getElementById('instant-target-val').value = val;
    colorScrollbox(val);
}

async function setInstantMode(mode) {
    try {
        // To apply instant mode, we modify the active config's initial-mode and save
        const newCfg = JSON.parse(JSON.stringify(currentConfig));
        if (!newCfg["Solax-BatteryControl"]) newCfg["Solax-BatteryControl"] = {};
        newCfg["Solax-BatteryControl"]["initial-mode"] = mode;
        
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
        if (!newCfg["Solax-BatteryControl"]) newCfg["Solax-BatteryControl"] = {};
        newCfg["Solax-BatteryControl"]["grid-target"] = targetVal;
        
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

// Modal & Dynamic Driver Card Helpers
function showAddDriverModal() {
    document.getElementById('add-driver-modal').style.display = 'flex';
}

function closeAddDriverModal() {
    document.getElementById('add-driver-modal').style.display = 'none';
}

function addDriverFromModal() {
    const type = document.getElementById('new-driver-type').value;
    renderDriverCard(type);
    closeAddDriverModal();
}

function renderDriverCard(type, data = {}) {
    const container = document.getElementById('drivers-list-container');
    const card = document.createElement('div');
    card.className = 'glass-card driver-card';
    card.setAttribute('data-driver-type', type);

    let content = '';
    if (type === 'Solax-Wifi') {
        const host = data.inverter || '';
        const poll = data.poll_period !== undefined ? data.poll_period : 10;
        const timeout = data.timeout !== undefined ? data.timeout : 5;
        content = `
            <div class="card-title">
                <span>SolaX Wi-Fi HTTP API</span>
                <button class="delete-btn" onclick="this.closest('.driver-card').remove()">Remove</button>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Inverter IP / Hostname</label>
                    <input type="text" class="driver-wifi-host" value="${host}" placeholder="e.g. 192.168.1.10">
                </div>
                <div class="form-group">
                    <label>Poll Period (s)</label>
                    <input type="number" class="driver-wifi-poll" value="${poll}">
                </div>
                <div class="form-group">
                    <label>Timeout (s)</label>
                    <input type="number" step="0.1" class="driver-wifi-timeout" value="${timeout}">
                </div>
            </div>
        `;
    } else if (type === 'Solax-Modbus') {
        const name = data.inverter || 'solax-modbus';
        const host = data.hostname || '';
        const poll = data.poll_period !== undefined ? data.poll_period : 10;
        const timeout = data.timeout !== undefined ? data.timeout : 5;
        const pwd = data.password !== undefined ? data.password : '';
        const avg = data.power_budget_avg_samples !== undefined ? data.power_budget_avg_samples : 30;
        content = `
            <div class="card-title">
                <span>SolaX Modbus TCP (Standard)</span>
                <button class="delete-btn" onclick="this.closest('.driver-card').remove()">Remove</button>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Inverter Name (Identifier)</label>
                    <input type="text" class="driver-modbus-name" value="${name}" placeholder="e.g. solax-modbus">
                </div>
                <div class="form-group">
                    <label>Inverter Host / IP (with optional port)</label>
                    <input type="text" class="driver-modbus-host" value="${host}" placeholder="e.g. 192.168.1.11:502">
                </div>
                <div class="form-group">
                    <label>Poll Period (s)</label>
                    <input type="number" class="driver-modbus-poll" value="${poll}">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Timeout (s)</label>
                    <input type="number" step="0.1" class="driver-modbus-timeout" value="${timeout}">
                </div>
                <div class="form-group">
                    <label>Installer Password</label>
                    <input type="number" class="driver-modbus-password" value="${pwd}" placeholder="Optional">
                </div>
                <div class="form-group">
                    <label>Power Budget Avg Samples</label>
                    <input type="number" class="driver-modbus-avg" value="${avg}">
                </div>
            </div>
        `;
    } else if (type === 'Solax-XHybrid-Modbus') {
        const name = data.inverter || 'solax-xhybrid';
        const host = data.hostname || '';
        const poll = data.poll_period !== undefined ? data.poll_period : 10;
        const timeout = data.timeout !== undefined ? data.timeout : 5;
        const pwd = data.password !== undefined ? data.password : '';
        const avg = data.power_budget_avg_samples !== undefined ? data.power_budget_avg_samples : 30;
        content = `
            <div class="card-title">
                <span>SolaX XHybrid Modbus TCP</span>
                <button class="delete-btn" onclick="this.closest('.driver-card').remove()">Remove</button>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Inverter Name (Identifier)</label>
                    <input type="text" class="driver-hybrid-name" value="${name}" placeholder="e.g. solax-xhybrid">
                </div>
                <div class="form-group">
                    <label>Inverter Host / IP (with optional port)</label>
                    <input type="text" class="driver-hybrid-host" value="${host}" placeholder="e.g. 192.168.1.11:502">
                </div>
                <div class="form-group">
                    <label>Poll Period (s)</label>
                    <input type="number" class="driver-hybrid-poll" value="${poll}">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Timeout (s)</label>
                    <input type="number" step="0.1" class="driver-hybrid-timeout" value="${timeout}">
                </div>
                <div class="form-group">
                    <label>Installer Password</label>
                    <input type="number" class="driver-hybrid-password" value="${pwd}" placeholder="Optional">
                </div>
                <div class="form-group">
                    <label>Power Budget Avg Samples</label>
                    <input type="number" class="driver-hybrid-avg" value="${avg}">
                </div>
            </div>
        `;
    } else if (type === 'SDM630Modbusv2') {
        const port = data.port || '';
        const poll = data.poll_period !== undefined ? data.poll_period : 1;
        const timeout = data.timeout !== undefined ? data.timeout : 1;
        const baud = data.baud !== undefined ? data.baud : 38400;
        const parity = data.parity || 'E';
        const stop = data.stopbits !== undefined ? data.stopbits : 1;
        content = `
            <div class="card-title">
                <span>Eastron SDM630 Serial Meter</span>
                <button class="delete-btn" onclick="this.closest('.driver-card').remove()">Remove</button>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Serial Port Path</label>
                    <input type="text" class="driver-sdm-port" value="${port}" placeholder="e.g. /dev/ttyUSB0">
                </div>
                <div class="form-group">
                    <label>Poll Period (s)</label>
                    <input type="number" class="driver-sdm-poll" value="${poll}">
                </div>
                <div class="form-group">
                    <label>Timeout (s)</label>
                    <input type="number" step="0.1" class="driver-sdm-timeout" value="${timeout}">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Baud Rate</label>
                    <input type="number" class="driver-sdm-baud" value="${baud}">
                </div>
                <div class="form-group">
                    <label>Parity</label>
                    <select class="driver-sdm-parity">
                        <option value="N" ${parity === 'N' ? 'selected' : ''}>None</option>
                        <option value="E" ${parity === 'E' ? 'selected' : ''}>Even</option>
                        <option value="O" ${parity === 'O' ? 'selected' : ''}>Odd</option>
                    </select>
                </div>
                <div class="form-group">
                    <label>Stop Bits</label>
                    <input type="number" class="driver-sdm-stop" value="${stop}">
                </div>
            </div>
        `;
    } else if (type === 'DTSU666') {
        const port = data.port || '';
        const poll = data.poll_period !== undefined ? data.poll_period : 1;
        const timeout = data.timeout !== undefined ? data.timeout : 1;
        const baud = data.baud !== undefined ? data.baud : 9600;
        const parity = data.parity || 'N';
        const stop = data.stopbits !== undefined ? data.stopbits : 1;
        content = `
            <div class="card-title">
                <span>Chint DTSU666 Serial Meter</span>
                <button class="delete-btn" onclick="this.closest('.driver-card').remove()">Remove</button>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Serial Port Path</label>
                    <input type="text" class="driver-dtsu-port" value="${port}" placeholder="e.g. /dev/ttyUSB0">
                </div>
                <div class="form-group">
                    <label>Poll Period (s)</label>
                    <input type="number" class="driver-dtsu-poll" value="${poll}">
                </div>
                <div class="form-group">
                    <label>Timeout (s)</label>
                    <input type="number" step="0.1" class="driver-dtsu-timeout" value="${timeout}">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Baud Rate</label>
                    <input type="number" class="driver-dtsu-baud" value="${baud}">
                </div>
                <div class="form-group">
                    <label>Parity</label>
                    <select class="driver-dtsu-parity">
                        <option value="N" ${parity === 'N' ? 'selected' : ''}>None</option>
                        <option value="E" ${parity === 'E' ? 'selected' : ''}>Even</option>
                        <option value="O" ${parity === 'O' ? 'selected' : ''}>Odd</option>
                    </select>
                </div>
                <div class="form-group">
                    <label>Stop Bits</label>
                    <input type="number" class="driver-dtsu-stop" value="${stop}">
                </div>
            </div>
        `;
    } else if (type === 'MQTTPowerMeter') {
        const name = data.meter_name || 'MainsMeter';
        const poll = data.poll_period !== undefined ? data.poll_period : 10;
        const broker = data.broker || '';
        const port = data.port !== undefined ? data.port : 1883;
        const user = data.username || '';
        const pass = data.password || '';
        const topicTotal = data.topic_total || '';
        const topicP1 = data.topic_phase1 || '';
        const topicP2 = data.topic_phase2 || '';
        const topicP3 = data.topic_phase3 || '';
        content = `
            <div class="card-title">
                <span>MQTT Custom Power Meter Bridge</span>
                <button class="delete-btn" onclick="this.closest('.driver-card').remove()">Remove</button>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Meter Name (Identifier)</label>
                    <input type="text" class="driver-mqtt-meter-name" value="${name}" placeholder="e.g. MainsMeter">
                </div>
                <div class="form-group">
                    <label>Poll Period (s)</label>
                    <input type="number" class="driver-mqtt-meter-poll" value="${poll}">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Broker Host / IP</label>
                    <input type="text" class="driver-mqtt-meter-broker" value="${broker}" placeholder="e.g. 192.168.1.5">
                </div>
                <div class="form-group">
                    <label>Broker Port</label>
                    <input type="number" class="driver-mqtt-meter-port" value="${port}">
                </div>
                <div class="form-group">
                    <label>Username</label>
                    <input type="text" class="driver-mqtt-meter-user" value="${user}" placeholder="Optional">
                </div>
                <div class="form-group">
                    <label>Password</label>
                    <input type="password" class="driver-mqtt-meter-pass" value="${pass}" placeholder="Optional">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>MQTT Topic: Total Power (W)</label>
                    <input type="text" class="driver-mqtt-meter-topic-total" value="${topicTotal}" placeholder="e.g. sensors/total_power">
                </div>
                <div class="form-group">
                    <label>MQTT Topic: Phase 1 Power (W)</label>
                    <input type="text" class="driver-mqtt-meter-topic-p1" value="${topicP1}" placeholder="Optional">
                </div>
                <div class="form-group">
                    <label>MQTT Topic: Phase 2 Power (W)</label>
                    <input type="text" class="driver-mqtt-meter-topic-p2" value="${topicP2}" placeholder="Optional">
                </div>
                <div class="form-group">
                    <label>MQTT Topic: Phase 3 Power (W)</label>
                    <input type="text" class="driver-mqtt-meter-topic-p3" value="${topicP3}" placeholder="Optional">
                </div>
            </div>
        `;
    } else if (type === 'MQTTInverter') {
        const name = data.inverter_name || 'aurora';
        const broker = data.broker || '';
        const port = data.port !== undefined ? data.port : 1883;
        const user = data.username || '';
        const pass = data.password || '';
        const topicPV1Power = data.topic_pv1_power || '';
        const topicPV2Power = data.topic_pv2_power || '';
        const topicPV1Volt = data.topic_pv1_voltage || '';
        const topicPV2Volt = data.topic_pv2_voltage || '';
        const topicPV1Curr = data.topic_pv1_current || '';
        const topicPV2Curr = data.topic_pv2_current || '';
        const topicGridVolt = data.topic_grid_voltage || '';
        const topicGridCurr = data.topic_grid_current || '';
        const topicGridPow = data.topic_grid_power || '';
        const topicFreq = data.topic_frequency || '';
        const topicTemp = data.topic_temperature || '';
        const topicEnergyToday = data.topic_energy_today || '';
        const topicEnergyTotal = data.topic_energy_total || '';
        const topicBatCap = data.topic_battery_capacity || '';
        const topicBatPow = data.topic_battery_power || '';

        content = `
            <div class="card-title">
                <span>MQTT Custom Inverter Bridge</span>
                <button class="delete-btn" onclick="this.closest('.driver-card').remove()">Remove</button>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Inverter Name (Identifier)</label>
                    <input type="text" class="driver-mqtt-inv-name" value="${name}" placeholder="e.g. aurora">
                </div>
                <div class="form-group">
                    <label>Broker Host / IP (Optional)</label>
                    <input type="text" class="driver-mqtt-inv-broker" value="${broker}" placeholder="e.g. 192.168.1.5">
                </div>
                <div class="form-group">
                    <label>Broker Port</label>
                    <input type="number" class="driver-mqtt-inv-port" value="${port}">
                </div>
            </div>
            <div class="form-row">
                <div class="form-group">
                    <label>Username</label>
                    <input type="text" class="driver-mqtt-inv-user" value="${user}" placeholder="Optional">
                </div>
                <div class="form-group">
                    <label>Password</label>
                    <input type="password" class="driver-mqtt-inv-pass" value="${pass}" placeholder="Optional">
                </div>
            </div>
            <h4 style="margin-top: 15px; margin-bottom: 5px; color: var(--primary);">Configurable MQTT Topics (Inputs)</h4>
            <div class="form-row" style="display: grid; grid-template-columns: 1fr 1fr; gap: 10px;">
                <div class="form-group">
                    <label>PV1 Power Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-pv1-power" value="${topicPV1Power}" placeholder="e.g. emon/aurora/power_in_1">
                </div>
                <div class="form-group">
                    <label>PV2 Power Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-pv2-power" value="${topicPV2Power}" placeholder="e.g. emon/aurora/power_in_2">
                </div>
                <div class="form-group">
                    <label>PV1 Voltage Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-pv1-voltage" value="${topicPV1Volt}">
                </div>
                <div class="form-group">
                    <label>PV2 Voltage Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-pv2-voltage" value="${topicPV2Volt}">
                </div>
                <div class="form-group">
                    <label>PV1 Current Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-pv1-current" value="${topicPV1Curr}">
                </div>
                <div class="form-group">
                    <label>PV2 Current Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-pv2-current" value="${topicPV2Curr}">
                </div>
                <div class="form-group">
                    <label>Grid Voltage Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-grid-voltage" value="${topicGridVolt}">
                </div>
                <div class="form-group">
                    <label>Grid Current Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-grid-current" value="${topicGridCurr}">
                </div>
                <div class="form-group">
                    <label>Grid Power Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-grid-power" value="${topicGridPow}">
                </div>
                <div class="form-group">
                    <label>Frequency Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-frequency" value="${topicFreq}">
                </div>
                <div class="form-group">
                    <label>Temperature Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-temperature" value="${topicTemp}">
                </div>
                <div class="form-group">
                    <label>Energy Today Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-energy-today" value="${topicEnergyToday}">
                </div>
                <div class="form-group">
                    <label>Energy Total Topic</label>
                    <input type="text" class="driver-mqtt-inv-topic-energy-total" value="${topicEnergyTotal}">
                </div>
                <div class="form-group" style="grid-column: span 2; display: grid; grid-template-columns: 1fr 1fr; gap: 10px;">
                    <div>
                        <label>Battery Capacity Topic</label>
                        <input type="text" class="driver-mqtt-inv-topic-battery-capacity" value="${topicBatCap}">
                    </div>
                    <div>
                        <label>Battery Power Topic</label>
                        <input type="text" class="driver-mqtt-inv-topic-battery-power" value="${topicBatPow}">
                    </div>
                </div>
            </div>
        `;
    }

    card.innerHTML = content;
    container.appendChild(card);
}

// Loading configuration to Forms
async function loadConfig(configData = null) {
    try {
        let config = configData;
        if (!config) {
            const r = await fetch('/api/config');
            if (!r.ok) return;
            config = await r.json();
        }
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

        // Clear dynamic drivers container
        const driversContainer = document.getElementById('drivers-list-container');
        driversContainer.innerHTML = '';

        // 1. Load Solax Wifi
        const wifi = config["Solax-Wifi"];
        if (wifi && wifi.inverters) {
            wifi.inverters.forEach(ip => {
                renderDriverCard('Solax-Wifi', {
                    inverter: ip,
                    poll_period: wifi["poll-period"] || wifi.poll_period || 10,
                    timeout: wifi.timeout || 5
                });
            });
        }

        // 2. Load Solax Modbus Standard
        const modbus = config["Solax-Modbus"];
        if (modbus && modbus.inverters) {
            modbus.inverters.forEach((name, idx) => {
                const host = (modbus.hostnames && modbus.hostnames[idx]) ? modbus.hostnames[idx] : '';
                renderDriverCard('Solax-Modbus', {
                    inverter: name,
                    hostname: host,
                    poll_period: modbus["poll-period"] || modbus.poll_period || 10,
                    timeout: modbus.timeout || 5,
                    password: modbus["installer-password"] || modbus.installer_password || '',
                    power_budget_avg_samples: modbus["power-budget-avg-samples"] || modbus.power_budget_avg_samples || 30
                });
            });
        }

        // 3. Load Solax XHybrid Modbus
        const hybrid = config["Solax-XHybrid-Modbus"];
        if (hybrid && hybrid.inverters) {
            hybrid.inverters.forEach((name, idx) => {
                const host = (hybrid.hostnames && hybrid.hostnames[idx]) ? hybrid.hostnames[idx] : '';
                renderDriverCard('Solax-XHybrid-Modbus', {
                    inverter: name,
                    hostname: host,
                    poll_period: hybrid["poll-period"] || hybrid.poll_period || 10,
                    timeout: hybrid.timeout || 5,
                    password: hybrid["installer-password"] || hybrid.installer_password || '',
                    power_budget_avg_samples: hybrid["power-budget-avg-samples"] || hybrid.power_budget_avg_samples || 30
                });
            });
        }

        // 4. Load SDM630
        const sdm = config.SDM630Modbusv2;
        if (sdm && sdm.ports) {
            sdm.ports.forEach(port => {
                renderDriverCard('SDM630Modbusv2', {
                    port: port,
                    poll_period: sdm["poll-period"] || sdm.poll_period || 1,
                    timeout: sdm.timeout || 1,
                    baud: sdm.baud || 38400,
                    parity: sdm.parity || 'E',
                    stopbits: sdm.stopbits || 1
                });
            });
        }

        // 5. Load DTSU666
        const dtsu = config.DTSU666;
        if (dtsu && dtsu.ports) {
            dtsu.ports.forEach(port => {
                renderDriverCard('DTSU666', {
                    port: port,
                    poll_period: dtsu["poll-period"] || dtsu.poll_period || 1,
                    timeout: dtsu.timeout || 1,
                    baud: dtsu.baud || 9600,
                    parity: dtsu.parity || 'N',
                    stopbits: dtsu.stopbits || 1
                });
            });
        }

        // 6. Load MQTT Custom Power Meter
        const mqMeter = config.MQTTPowerMeter;
        if (mqMeter && mqMeter.meters) {
            mqMeter.meters.forEach(meterName => {
                const mDev = mqMeter[meterName] || mqMeter.meter_devices?.[meterName] || {};
                renderDriverCard('MQTTPowerMeter', {
                    meter_name: meterName,
                    poll_period: mqMeter["poll-period"] || mqMeter.poll_period || 10,
                    broker: mDev.broker || '',
                    port: mDev.port || 1883,
                    username: mDev.username || '',
                    password: mDev.password || '',
                    topic_total: mDev.topic_total || mDev["topic-total"] || '',
                    topic_phase1: mDev.topic_phase1 || mDev["topic-phase1"] || '',
                    topic_phase2: mDev.topic_phase2 || mDev["topic-phase2"] || '',
                    topic_phase3: mDev.topic_phase3 || mDev["topic-phase3"] || ''
                });
            });
        }

        // 7. Load MQTT Custom Inverters
        const mqInverters = config.MQTTInverter;
        if (mqInverters && mqInverters.inverters) {
            mqInverters.inverters.forEach(invName => {
                const iDev = mqInverters[invName] || mqInverters.inverter_devices?.[invName] || {};
                renderDriverCard('MQTTInverter', {
                    inverter_name: invName,
                    broker: iDev.broker || '',
                    port: iDev.port || 1883,
                    username: iDev.username || '',
                    password: iDev.password || '',
                    topic_pv1_power: iDev.topic_pv1_power || iDev["topic-pv1-power"] || '',
                    topic_pv2_power: iDev.topic_pv2_power || iDev["topic-pv2-power"] || '',
                    topic_pv1_voltage: iDev.topic_pv1_voltage || iDev["topic-pv1-voltage"] || '',
                    topic_pv2_voltage: iDev.topic_pv2_voltage || iDev["topic-pv2-voltage"] || '',
                    topic_pv1_current: iDev.topic_pv1_current || iDev["topic-pv1-current"] || '',
                    topic_pv2_current: iDev.topic_pv2_current || iDev["topic-pv2-current"] || '',
                    topic_grid_voltage: iDev.topic_grid_voltage || iDev["topic-grid-voltage"] || '',
                    topic_grid_current: iDev.topic_grid_current || iDev["topic-grid-current"] || '',
                    topic_grid_power: iDev.topic_grid_power || iDev["topic-grid-power"] || '',
                    topic_frequency: iDev.topic_frequency || iDev["topic-frequency"] || '',
                    topic_temperature: iDev.topic_temperature || iDev["topic-temperature"] || '',
                    topic_energy_today: iDev.topic_energy_today || iDev["topic-energy-today"] || '',
                    topic_energy_total: iDev.topic_energy_total || iDev["topic-energy-total"] || '',
                    topic_battery_capacity: iDev.topic_battery_capacity || iDev["topic-battery-capacity"] || '',
                    topic_battery_power: iDev.topic_battery_power || iDev["topic-battery-power"] || ''
                });
            });
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
            const gridTarget = bat["grid-target"] || 0.0;
            document.getElementById('instant-target-val').value = gridTarget;
            document.getElementById('instant-target-slider').value = gridTarget;
            colorScrollbox(gridTarget);

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

        // Load Tariff Settings
        const tariff = (bat && bat.tariff) ? bat.tariff : null;
        if (tariff) {
            const tType = tariff.type;
            document.getElementById('tariff-type').value = tType;
            toggleTariffType(tType);

            if (tType === 'flat') {
                document.getElementById('flat-import-rate').value = tariff["import-rate"] || '';
                document.getElementById('flat-export-rate').value = tariff["export-rate"] || '';
            } else if (tType === 'tou') {
                const tariffList = document.getElementById('tariff-tou-periods-list');
                tariffList.innerHTML = '';
                if (tariff.periods) {
                    tariff.periods.forEach(p => {
                        renderTariffTOUPeriodCard(p.name, {
                            start: p.start,
                            end: p.end,
                            import_rate: p["import-rate"],
                            export_rate: p["export-rate"]
                        });
                    });
                }
            } else if (tType === 'amber') {
                document.getElementById('amber-api-key').value = tariff["api-key"] || '';
                document.getElementById('amber-site-id').value = tariff["site-id"] || '';
                document.getElementById('amber-api-url').value = tariff["api-url"] || '';
                document.getElementById('amber-neg-export-prevent').checked = tariff["negative-export-prevent"] === true;
                
                const lpc = tariff["low-price-charge"] === true;
                document.getElementById('amber-low-price-charge').checked = lpc;
                toggleFormSection('amber-low-price-group', lpc);
                document.getElementById('amber-low-price-threshold').value = tariff["low-price-threshold"] || '';

                const hpd = tariff["high-price-discharge"] === true;
                document.getElementById('amber-high-price-discharge').checked = hpd;
                toggleFormSection('amber-high-price-group', hpd);
                document.getElementById('amber-high-price-threshold').value = tariff["high-price-threshold"] || '';
            }
        } else {
            document.getElementById('tariff-type').value = 'none';
            toggleTariffType('none');
        }

        // Load Demand settings
        const demand = (bat && bat.demand) ? bat.demand : null;
        if (demand) {
            document.getElementById('demand-start').value = demand.start || '';
            document.getElementById('demand-end').value = demand.end || '';
            document.getElementById('demand-rate').value = demand.rate || '';
        } else {
            document.getElementById('demand-start').value = '';
            document.getElementById('demand-end').value = '';
            document.getElementById('demand-rate').value = '';
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
        updateHeaderButtons();
    } catch (e) {
        console.error("Failed to load config backend", e);
    }
}

function isConfigPopulated(config) {
    if (!config) return false;

    // Check if any hardware driver has configured items
    if (config["Solax-Wifi"] && config["Solax-Wifi"].inverters && config["Solax-Wifi"].inverters.length > 0) return true;
    if (config["Solax-Modbus"] && config["Solax-Modbus"].inverters && config["Solax-Modbus"].inverters.length > 0) return true;
    if (config["Solax-XHybrid-Modbus"] && config["Solax-XHybrid-Modbus"].inverters && config["Solax-XHybrid-Modbus"].inverters.length > 0) return true;
    if (config.SDM630Modbusv2 && config.SDM630Modbusv2.ports && config.SDM630Modbusv2.ports.length > 0) return true;
    if (config.DTSU666 && config.DTSU666.ports && config.DTSU666.ports.length > 0) return true;
    if (config.MQTTPowerMeter && config.MQTTPowerMeter.meters && config.MQTTPowerMeter.meters.length > 0) return true;
    if (config.MQTTInverter && config.MQTTInverter.inverters && config.MQTTInverter.inverters.length > 0) return true;

    // Check if emoncms is configured
    if (config.emoncms && (config.emoncms.server || config.emoncms.api_key)) return true;

    // Check if InfluxDB is configured
    if (config.influx && (config.influx.influx_url || config.influx.influx_database)) return true;

    // Check if Battery Control / periods / tariff are configured
    const bat = config["Solax-BatteryControl"];
    if (bat) {
        if (bat.inverter && Object.keys(bat.inverter).length > 0) return true;
        if (bat.period && Object.keys(bat.period).length > 0) return true;
        if (bat.tariff && bat.tariff.type && bat.tariff.type !== 'none') return true;
        if (bat.demand && bat.demand.start && bat.demand.end) return true;
    }

    // Check if MQTT is customized from default empty values
    const mqtt = config.MQTT;
    if (mqtt) {
        if (mqtt.broker && mqtt.broker !== "127.0.0.1") return true;
        if (mqtt.port && mqtt.port !== 1883) return true;
        if (mqtt.username || mqtt.password) return true;
        if (mqtt["base-topic"] && mqtt["base-topic"] !== "sensors") return true;
    }

    return false;
}

function updateHeaderButtons() {
    const importBtn = document.querySelector('.btn-import');
    if (importBtn) {
        if (isConfigPopulated(currentConfig)) {
            importBtn.style.display = 'none';
        } else {
            importBtn.style.display = 'inline-block';
        }
    }
}

// Dynamic Card Rendering helpers
function renderInverterConstraintCard(name, inv) {
    const listDiv = document.getElementById('inverters-constraints-list');
    const card = document.createElement('div');
    card.className = 'list-item-card inverter-constraint-card';
    
    let calcCapText = "N/A";
    if (typeof lastStatusData !== 'undefined' && lastStatusData && lastStatusData.inverters && lastStatusData.inverters[name]) {
        const invStatus = lastStatusData.inverters[name];
        if (invStatus.calculated_battery_capacity !== undefined && invStatus.calculated_battery_capacity !== null) {
            calcCapText = `${invStatus.calculated_battery_capacity.toFixed(2)} kWh`;
        }
    }

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
                <div class="form-group">
                    <label>Battery Capacity (kWh)</label>
                    <input type="number" step="0.1" class="inv-battery-capacity" value="${inv["battery-capacity"] || inv.battery_capacity || 0.0}">
                </div>
                <div class="form-group">
                    <label>Calculated Battery Capacity</label>
                    <input type="text" class="inv-calc-capacity" value="${calcCapText}" readonly style="background: rgba(255,255,255,0.05); color: #ccc;">
                </div>
                <div class="form-group">
                    <label>Max Charge (%)</label>
                    <input type="number" min="0" max="100" class="inv-max-charge-pct" value="${inv["max-charge-pct"] || inv.max_charge_pct || 100}">
                </div>
                <div class="form-group">
                    <label>Min Charge (%)</label>
                    <input type="number" min="0" max="100" class="inv-min-charge-pct" value="${inv["min-charge-pct"] || inv.min_charge_pct || 10}">
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
        "control-grid-power": false,
        "battery-capacity": 0.0,
        "max-charge-pct": 100,
        "min-charge-pct": 10
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

function toggleTariffType(type) {
    document.querySelectorAll('.tariff-section').forEach(s => s.style.display = 'none');
    if (type === 'flat') {
        document.getElementById('tariff-section-flat').style.display = 'block';
    } else if (type === 'tou') {
        document.getElementById('tariff-section-tou').style.display = 'block';
    } else if (type === 'amber') {
        document.getElementById('tariff-section-amber').style.display = 'block';
    }
}

function renderTariffTOUPeriodCard(pName, per) {
    const container = document.getElementById('tariff-tou-periods-list');
    const card = document.createElement('div');
    card.className = 'list-item-card tariff-tou-period-card';
    card.innerHTML = `
        <div style="flex: 1; display: flex; flex-direction: column; gap: 10px;">
            <div class="form-row">
                <div class="form-group">
                    <label>Period Name</label>
                    <input type="text" class="tariff-period-name" value="${pName}">
                </div>
                <div class="form-group">
                    <label>Start Time</label>
                    <input type="text" class="tariff-period-start" value="${per.start || '00:00:00'}">
                </div>
                <div class="form-group">
                    <label>End Time</label>
                    <input type="text" class="tariff-period-end" value="${per.end || '23:59:59'}">
                </div>
                <div class="form-group">
                    <label>Import Rate (c/kWh)</label>
                    <input type="number" step="0.01" class="tariff-period-import" value="${per.import_rate || 0.0}">
                </div>
                <div class="form-group">
                    <label>Export Rate (c/kWh)</label>
                    <input type="number" step="0.01" class="tariff-period-export" value="${per.export_rate || 0.0}">
                </div>
            </div>
        </div>
        <button class="sub-btn danger" style="margin-left: 20px;" onclick="this.parentElement.remove()">Remove</button>
    `;
    container.appendChild(card);
}

function addTariffTOUPeriod() {
    renderTariffTOUPeriodCard('Peak', {
        start: '00:00:00',
        end: '23:59:59',
        import_rate: 30.0,
        export_rate: 10.0
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

    let wifiConfig = null;
    let modbusConfig = null;
    let hybridConfig = null;
    let sdmConfig = null;
    let dtsuConfig = null;
    let mqttMeterConfig = null;
    let mqttInverterConfig = null;

    document.querySelectorAll('.driver-card').forEach(card => {
        const type = card.getAttribute('data-driver-type');
        if (type === 'Solax-Wifi') {
            const host = card.querySelector('.driver-wifi-host').value.trim();
            const poll = parseInt(card.querySelector('.driver-wifi-poll').value) || 10;
            const timeout = parseFloat(card.querySelector('.driver-wifi-timeout').value) || 5;
            if (host) {
                if (!wifiConfig) {
                    wifiConfig = {
                        "poll-period": poll,
                        timeout: timeout,
                        inverters: []
                    };
                }
                wifiConfig.inverters.push(host);
            }
        } else if (type === 'Solax-Modbus') {
            const name = card.querySelector('.driver-modbus-name').value.trim();
            const host = card.querySelector('.driver-modbus-host').value.trim();
            const poll = parseInt(card.querySelector('.driver-modbus-poll').value) || 10;
            const timeout = parseFloat(card.querySelector('.driver-modbus-timeout').value) || 5;
            const pwdVal = card.querySelector('.driver-modbus-password').value.trim();
            const pwd = pwdVal ? parseInt(pwdVal) : null;
            const avg = parseInt(card.querySelector('.driver-modbus-avg').value) || 30;
            if (name && host) {
                if (!modbusConfig) {
                    modbusConfig = {
                        "poll-period": poll,
                        timeout: timeout,
                        "installer-password": pwd,
                        "power-budget-avg-samples": avg,
                        inverters: [],
                        hostnames: []
                    };
                }
                modbusConfig.inverters.push(name);
                modbusConfig.hostnames.push(host);
            }
        } else if (type === 'Solax-XHybrid-Modbus') {
            const name = card.querySelector('.driver-hybrid-name').value.trim();
            const host = card.querySelector('.driver-hybrid-host').value.trim();
            const poll = parseInt(card.querySelector('.driver-hybrid-poll').value) || 10;
            const timeout = parseFloat(card.querySelector('.driver-hybrid-timeout').value) || 5;
            const pwdVal = card.querySelector('.driver-hybrid-password').value.trim();
            const pwd = pwdVal ? parseInt(pwdVal) : null;
            const avg = parseInt(card.querySelector('.driver-hybrid-avg').value) || 30;
            if (name && host) {
                if (!hybridConfig) {
                    hybridConfig = {
                        "poll-period": poll,
                        timeout: timeout,
                        "installer-password": pwd,
                        "power-budget-avg-samples": avg,
                        inverters: [],
                        hostnames: []
                    };
                }
                hybridConfig.inverters.push(name);
                hybridConfig.hostnames.push(host);
            }
        } else if (type === 'SDM630Modbusv2') {
            const port = card.querySelector('.driver-sdm-port').value.trim();
            const poll = parseInt(card.querySelector('.driver-sdm-poll').value) || 1;
            const timeout = parseFloat(card.querySelector('.driver-sdm-timeout').value) || 1;
            const baud = parseInt(card.querySelector('.driver-sdm-baud').value) || 38400;
            const parity = card.querySelector('.driver-sdm-parity').value;
            const stop = parseInt(card.querySelector('.driver-sdm-stop').value) || 1;
            if (port) {
                if (!sdmConfig) {
                    sdmConfig = {
                        "poll-period": poll,
                        timeout: timeout,
                        baud: baud,
                        parity: parity,
                        stopbits: stop,
                        ports: []
                    };
                }
                sdmConfig.ports.push(port);
            }
        } else if (type === 'DTSU666') {
            const port = card.querySelector('.driver-dtsu-port').value.trim();
            const poll = parseInt(card.querySelector('.driver-dtsu-poll').value) || 1;
            const timeout = parseFloat(card.querySelector('.driver-dtsu-timeout').value) || 1;
            const baud = parseInt(card.querySelector('.driver-dtsu-baud').value) || 9600;
            const parity = card.querySelector('.driver-dtsu-parity').value;
            const stop = parseInt(card.querySelector('.driver-dtsu-stop').value) || 1;
            if (port) {
                if (!dtsuConfig) {
                    dtsuConfig = {
                        "poll-period": poll,
                        timeout: timeout,
                        baud: baud,
                        parity: parity,
                        stopbits: stop,
                        ports: []
                    };
                }
                dtsuConfig.ports.push(port);
            }
        } else if (type === 'MQTTPowerMeter') {
            const name = card.querySelector('.driver-mqtt-meter-name').value.trim();
            const poll = parseInt(card.querySelector('.driver-mqtt-meter-poll').value) || 10;
            const broker = card.querySelector('.driver-mqtt-meter-broker').value.trim();
            const port = parseInt(card.querySelector('.driver-mqtt-meter-port').value) || 1883;
            const user = card.querySelector('.driver-mqtt-meter-user').value.trim() || null;
            const pass = card.querySelector('.driver-mqtt-meter-pass').value.trim() || null;
            const topicTotal = card.querySelector('.driver-mqtt-meter-topic-total').value.trim() || null;
            const topicP1 = card.querySelector('.driver-mqtt-meter-topic-p1').value.trim() || null;
            const topicP2 = card.querySelector('.driver-mqtt-meter-topic-p2').value.trim() || null;
            const topicP3 = card.querySelector('.driver-mqtt-meter-topic-p3').value.trim() || null;
            if (name && broker) {
                if (!mqttMeterConfig) {
                    mqttMeterConfig = {
                        "poll-period": poll,
                        meters: []
                    };
                }
                mqttMeterConfig.meters.push(name);
                mqttMeterConfig[name] = {
                    broker: broker,
                    port: port,
                    username: user,
                    password: pass,
                    topic_total: topicTotal,
                    topic_phase1: topicP1,
                    topic_phase2: topicP2,
                    topic_phase3: topicP3
                };
            }
        } else if (type === 'MQTTInverter') {
            const name = card.querySelector('.driver-mqtt-inv-name').value.trim();
            const broker = card.querySelector('.driver-mqtt-inv-broker').value.trim() || null;
            const port = parseInt(card.querySelector('.driver-mqtt-inv-port').value) || 1883;
            const user = card.querySelector('.driver-mqtt-inv-user').value.trim() || null;
            const pass = card.querySelector('.driver-mqtt-inv-pass').value.trim() || null;
            const topicPV1Power = card.querySelector('.driver-mqtt-inv-topic-pv1-power').value.trim() || null;
            const topicPV2Power = card.querySelector('.driver-mqtt-inv-topic-pv2-power').value.trim() || null;
            const topicPV1Volt = card.querySelector('.driver-mqtt-inv-topic-pv1-voltage').value.trim() || null;
            const topicPV2Volt = card.querySelector('.driver-mqtt-inv-topic-pv2-voltage').value.trim() || null;
            const topicPV1Curr = card.querySelector('.driver-mqtt-inv-topic-pv1-current').value.trim() || null;
            const topicPV2Curr = card.querySelector('.driver-mqtt-inv-topic-pv2-current').value.trim() || null;
            const topicGridVolt = card.querySelector('.driver-mqtt-inv-topic-grid-voltage').value.trim() || null;
            const topicGridCurr = card.querySelector('.driver-mqtt-inv-topic-grid-current').value.trim() || null;
            const topicGridPow = card.querySelector('.driver-mqtt-inv-topic-grid-power').value.trim() || null;
            const topicFreq = card.querySelector('.driver-mqtt-inv-topic-frequency').value.trim() || null;
            const topicTemp = card.querySelector('.driver-mqtt-inv-topic-temperature').value.trim() || null;
            const topicEnergyToday = card.querySelector('.driver-mqtt-inv-topic-energy-today').value.trim() || null;
            const topicEnergyTotal = card.querySelector('.driver-mqtt-inv-topic-energy-total').value.trim() || null;
            const topicBatCap = card.querySelector('.driver-mqtt-inv-topic-battery-capacity').value.trim() || null;
            const topicBatPow = card.querySelector('.driver-mqtt-inv-topic-battery-power').value.trim() || null;

            if (name) {
                if (!mqttInverterConfig) {
                    mqttInverterConfig = {
                        inverters: []
                    };
                }
                mqttInverterConfig.inverters.push(name);
                mqttInverterConfig[name] = {
                    broker: broker,
                    port: port,
                    username: user,
                    password: pass,
                    topic_pv1_power: topicPV1Power,
                    topic_pv2_power: topicPV2Power,
                    topic_pv1_voltage: topicPV1Volt,
                    topic_pv2_voltage: topicPV2Volt,
                    topic_pv1_current: topicPV1Curr,
                    topic_pv2_current: topicPV2Curr,
                    topic_grid_voltage: topicGridVolt,
                    topic_grid_current: topicGridCurr,
                    topic_grid_power: topicGridPow,
                    topic_frequency: topicFreq,
                    topic_temperature: topicTemp,
                    topic_energy_today: topicEnergyToday,
                    topic_energy_total: topicEnergyTotal,
                    topic_battery_capacity: topicBatCap,
                    topic_battery_power: topicBatPow
                };
            }
        }
    });

    cfg["Solax-Wifi"] = wifiConfig;
    cfg["Solax-Modbus"] = modbusConfig;
    cfg["Solax-XHybrid-Modbus"] = hybridConfig;
    cfg.SDM630Modbusv2 = sdmConfig;
    cfg.DTSU666 = dtsuConfig;
    cfg.MQTTPowerMeter = mqttMeterConfig;
    cfg.MQTTInverter = mqttInverterConfig;

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
                    "battery-capacity": parseFloat(card.querySelector('.inv-battery-capacity').value) || 0.0,
                    "max-charge-pct": parseInt(card.querySelector('.inv-max-charge-pct').value) || 100,
                    "min-charge-pct": parseInt(card.querySelector('.inv-min-charge-pct').value) || 10,
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

        // Compile Tariff
        const tType = document.getElementById('tariff-type').value;
        if (tType === 'flat') {
            cfg["Solax-BatteryControl"].tariff = {
                type: 'flat',
                "import-rate": parseFloat(document.getElementById('flat-import-rate').value) || 0.0,
                "export-rate": parseFloat(document.getElementById('flat-export-rate').value) || 0.0
            };
        } else if (tType === 'tou') {
            const periods = [];
            document.querySelectorAll('.tariff-tou-period-card').forEach(card => {
                const name = card.querySelector('.tariff-period-name').value.trim();
                if (name) {
                    periods.push({
                        name: name,
                        start: card.querySelector('.tariff-period-start').value.trim() || "00:00:00",
                        end: card.querySelector('.tariff-period-end').value.trim() || "23:59:59",
                        "import-rate": parseFloat(card.querySelector('.tariff-period-import').value) || 0.0,
                        "export-rate": parseFloat(card.querySelector('.tariff-period-export').value) || 0.0
                    });
                }
            });
            cfg["Solax-BatteryControl"].tariff = {
                type: 'tou',
                periods: periods
            };
        } else if (tType === 'amber') {
            const apiUrlVal = document.getElementById('amber-api-url').value.trim();
            cfg["Solax-BatteryControl"].tariff = {
                type: 'amber',
                "api-key": document.getElementById('amber-api-key').value.trim(),
                "site-id": document.getElementById('amber-site-id').value.trim(),
                "api-url": apiUrlVal || null,
                "negative-export-prevent": document.getElementById('amber-neg-export-prevent').checked,
                "low-price-charge": document.getElementById('amber-low-price-charge').checked,
                "low-price-threshold": parseFloat(document.getElementById('amber-low-price-threshold').value) || 0.0,
                "high-price-discharge": document.getElementById('amber-high-price-discharge').checked,
                "high-price-threshold": parseFloat(document.getElementById('amber-high-price-threshold').value) || 0.0
            };
        } else {
            cfg["Solax-BatteryControl"].tariff = null;
        }

        // Compile Demand Settings
        const dStart = document.getElementById('demand-start').value.trim();
        const dEnd = document.getElementById('demand-end').value.trim();
        const dRateRaw = document.getElementById('demand-rate').value.trim();
        const dRate = parseFloat(dRateRaw);
        if (dStart && dEnd && !isNaN(dRate)) {
            cfg["Solax-BatteryControl"].demand = {
                start: dStart,
                end: dEnd,
                rate: dRate
            };
        } else {
            cfg["Solax-BatteryControl"].demand = null;
        }
    } else {
        cfg["Solax-BatteryControl"] = null;
    }

    // EmonCMS
    if (document.getElementById('emon-enable').checked) {
        cfg.emoncms = {
            server: document.getElementById('emon-server').value,
            timeout: parseFloat(document.getElementById('emon-timeout').value) || 5,
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

function triggerImportConfig() {
    document.getElementById('import-config-file').click();
}

async function handleImportConfig(event) {
    const file = event.target.files[0];
    if (!file) return;

    // Reset input so upload can be re-triggered for the same file
    event.target.value = '';

    const reader = new FileReader();
    reader.onload = async function(e) {
        const tomlText = e.target.result;
        try {
            const resp = await fetch('/api/config/import', {
                method: 'POST',
                headers: { 'Content-Type': 'text/plain' },
                body: tomlText
            });
            if (resp.ok) {
                const parsedConfig = await resp.json();
                loadConfig(parsedConfig);
                alert("Configuration imported successfully! Review the imported settings and click 'Apply Changes' to save them permanently.");
            } else {
                const err = await resp.text();
                alert(`Failed to parse configuration: ${err}`);
            }
        } catch (err) {
            alert(`Error importing configuration: ${err}`);
        }
    };
    reader.onerror = function() {
        alert("Error reading file.");
    };
    reader.readAsText(file);
}

async function testMqttConnection() {
    const btn = document.getElementById('btn-mqtt-test');
    const resultEl = document.getElementById('mqtt-test-result');
    
    const broker = document.getElementById('mqtt-broker').value.trim();
    if (!broker) {
        resultEl.innerText = "Error: Broker Hostname / IP is required.";
        resultEl.style.color = "var(--danger)";
        return;
    }
    
    const port = parseInt(document.getElementById('mqtt-port').value, 10) || 1883;
    const username = document.getElementById('mqtt-username').value.trim() || null;
    const password = document.getElementById('mqtt-password').value.trim() || null;
    const baseTopic = document.getElementById('mqtt-base').value.trim() || "sensors";
    const haDiscovery = document.getElementById('mqtt-ha-discovery').checked;
    const haPrefix = document.getElementById('mqtt-ha-prefix').value.trim() || "homeassistant";
    
    // Disable button & show loading status
    btn.disabled = true;
    const originalText = btn.innerText;
    btn.innerText = "Testing...";
    
    resultEl.innerText = "Connecting & testing permissions...";
    resultEl.style.color = "var(--text-muted)";
    
    try {
        const payload = {
            "broker": broker,
            "port": port,
            "base-topic": baseTopic,
            "username": username,
            "password": password,
            "home-assistant-discovery": haDiscovery,
            "home-assistant-prefix": haPrefix
        };
        
        const response = await fetch('/api/mqtt/test', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify(payload)
        });
        
        if (!response.ok) {
            const errText = await response.text();
            throw new Error(errText || `Server returned status ${response.status}`);
        }
        
        const data = await response.json();
        if (data.connected && !data.error) {
            resultEl.innerText = "Success: MQTT connection and all permissions verified!";
            resultEl.style.color = "var(--accent)";
        } else {
            resultEl.innerText = `Failed: ${data.error || "Unknown error"}`;
            resultEl.style.color = "var(--danger)";
        }
    } catch (e) {
        resultEl.innerText = `Connection test failed: ${e.message}`;
        resultEl.style.color = "var(--danger)";
    } finally {
        btn.disabled = false;
        btn.innerText = originalText;
    }
}

// Update age counters for mains and inverters
function updateAgeCounters() {
    const now = Math.floor(Date.now() / 1000);
    
    // Update mains/grid age
    const mainsUpdatedEl = document.getElementById('stat-mains-updated');
    if (mainsUpdatedEl) {
        const tsAttr = mainsUpdatedEl.getAttribute('data-timestamp');
        if (tsAttr) {
            const ts = parseInt(tsAttr, 10);
            const diff = now - ts;
            if (diff >= 0) {
                mainsUpdatedEl.innerText = `last updated: ${diff} second${diff === 1 ? '' : 's'} ago`;
            } else {
                mainsUpdatedEl.innerText = `last updated: just now`;
            }
        } else {
            mainsUpdatedEl.innerText = '';
        }
    }
    
    // Update inverter ages
    document.querySelectorAll('.inverter-age').forEach(el => {
        const tsAttr = el.getAttribute('data-timestamp');
        if (tsAttr) {
            const ts = parseInt(tsAttr, 10);
            const diff = now - ts;
            if (diff >= 0) {
                el.innerText = `last updated: ${diff} second${diff === 1 ? '' : 's'} ago`;
            } else {
                el.innerText = `last updated: just now`;
            }
        } else {
            el.innerText = '';
        }
    });
}

async function runHistoricalSimulation() {
    const range = document.getElementById('sim-range').value;
    const btn = document.getElementById('btn-run-simulation');
    const loadingDiv = document.getElementById('sim-loading');
    const progressBar = document.getElementById('sim-progress-bar');
    const etaText = document.getElementById('sim-eta');
    const loadingText = document.getElementById('sim-loading-text');
    const resultsDiv = document.getElementById('sim-results');
    const badge = document.getElementById('sim-period-badge');
    const tableBody = document.getElementById('sim-table-body');

    btn.disabled = true;
    loadingDiv.style.display = 'block';
    resultsDiv.style.display = 'none';
    progressBar.style.width = '0%';
    etaText.innerText = 'ETA: Estimating...';
    loadingText.innerText = 'Initializing simulation...';

    const eventSource = new EventSource(`/api/simulation/run?range=${range}`);

    eventSource.onmessage = (event) => {
        try {
            const msg = JSON.parse(event.data);
            if (msg.type === 'Progress') {
                const pct = msg.percent;
                progressBar.style.width = `${pct.toFixed(0)}%`;
                loadingText.innerText = `Simulating battery control models... (${pct.toFixed(0)}%)`;
                if (msg.eta_seconds > 0) {
                    const sec = Math.ceil(msg.eta_seconds);
                    if (sec >= 60) {
                        const mins = Math.floor(sec / 60);
                        const secs = sec % 60;
                        etaText.innerText = `ETA: ~${mins}m ${secs}s remaining`;
                    } else {
                        etaText.innerText = `ETA: ~${sec}s remaining`;
                    }
                } else {
                    etaText.innerText = 'ETA: Estimating...';
                }
            } else if (msg.type === 'Result') {
                eventSource.close();
                btn.disabled = false;
                loadingDiv.style.display = 'none';

                const data = msg.response;
                badge.innerText = `${data.start_date.substring(0, 10)} to ${data.end_date.substring(0, 10)} (${data.records_simulated.toLocaleString()} records)`;

                const fmtVal = (val) => {
                    const sign = val < 0 ? '-' : '';
                    return `${sign}$${Math.abs(val).toFixed(2)}`;
                };

                const fmtSavings = (val, isBaseline = false) => {
                    if (isBaseline) return '-';
                    const style = val >= 0 ? 'color: var(--accent); font-weight: 600;' : 'color: var(--danger); font-weight: 600;';
                    const sign = val < 0 ? '-' : '';
                    return `<span style="${style}">${sign}$${Math.abs(val).toFixed(2)}</span>`;
                };

                const models = [
                    { name: "Scenario A: No Battery", key: "no_battery", isBaseline: true },
                    { name: "Scenario B: Baseline (Solar Self-Consumption)", key: "baseline" },
                    { name: "Scenario C: Auto (Period-Aware Regulation)", key: "auto" },
                    { name: "Scenario D: Smart Heuristic", key: "smart_heuristic" },
                    { name: "Scenario E: Look-Ahead MPC", key: "lookahead_mpc" },
                    { name: "Scenario F: Adaptive Peak Shaving", key: "adaptive_peak" }
                ];

                const noBatteryBill = data.no_battery.net_bill;

                tableBody.innerHTML = models.map(m => {
                    const mData = data[m.key];
                    const netSavings = noBatteryBill - mData.net_bill;
                    
                    return `
                        <tr style="border-bottom: 1px solid var(--border-color);">
                            <td style="padding: 12px 15px; font-weight: 500; color: #fff;">${m.name}</td>
                            <td style="padding: 12px 15px; text-align: right;">${data.total_usage_kwh.toFixed(1)}</td>
                            <td style="padding: 12px 15px; text-align: right;">${data.total_solar_kwh.toFixed(1)}</td>
                            <td style="padding: 12px 15px; text-align: right;">${mData.import_kwh.toFixed(1)}</td>
                            <td style="padding: 12px 15px; text-align: right;">${mData.export_kwh.toFixed(1)}</td>
                            <td style="padding: 12px 15px; text-align: right;">${m.isBaseline ? '-' : mData.cycles.toFixed(1)}</td>
                            <td style="padding: 12px 15px; text-align: right;">${fmtVal(mData.energy_cost)}</td>
                            <td style="padding: 12px 15px; text-align: right;">${fmtVal(mData.demand_charges)}</td>
                            <td style="padding: 12px 15px; text-align: right; font-weight: 600; color: var(--primary);">${fmtVal(mData.net_bill)}</td>
                            <td style="padding: 12px 15px; text-align: right;">${fmtSavings(netSavings, m.isBaseline)}</td>
                        </tr>
                    `;
                }).join('');

                resultsDiv.style.display = 'block';
            } else if (msg.type === 'Error') {
                eventSource.close();
                btn.disabled = false;
                loadingDiv.style.display = 'none';
                alert("Failed to run historical simulation: " + msg.message);
            }
        } catch (e) {
            console.error("Failed to parse SSE data", e);
        }
    };

    eventSource.onerror = (err) => {
        eventSource.close();
        btn.disabled = false;
        loadingDiv.style.display = 'none';
        alert("Failed to run historical simulation: Connection closed unexpectedly.");
        console.error("SSE error", err);
    };
}

// Initialize UI
window.addEventListener('load', () => {
    loadConfig();
    fetchStatus();
    setInterval(fetchStatus, 3000); // Poll status every 3s
    setInterval(updateAgeCounters, 1000); // Update elapsed age counters every 1s
});
