use crate::config::{MqttBrokerConfig, SolaxModbusConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::{Event, Packet, QoS};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use tokio_modbus::client::{Context, Reader, Writer};
use tokio_modbus::prelude::*;
use tokio_util::sync::CancellationToken;

async fn resolve_address(host: &str) -> std::io::Result<std::net::SocketAddr> {
    let mut addrs = tokio::net::lookup_host(host).await?;
    if let Some(addr) = addrs.next() {
        Ok(addr)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("Could not resolve address: {}", host),
        ))
    }
}

pub async fn run_solax_modbus_driver(
    inverter_name: String,
    hostname: String,
    config: SolaxModbusConfig,
    mqtt_config: MqttBrokerConfig,
    tx_telemetry: tokio::sync::mpsc::Sender<crate::dispatch_manager::TelemetryBatch>,
    cancel_token: CancellationToken,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = format!("powerscraper-modbus-{}", inverter_name);
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    let ctx_opt: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
    let requested_battery_power = Arc::new(Mutex::new(0i32));

    // Handle handshake if installer_password is configured
    if let Some(password) = config.installer_password {
        let mut lock = ctx_opt.lock().await;
        if let Ok(addr) = resolve_address(&hostname).await {
            if let Ok(mut ctx) = tcp::connect(addr).await {
                let _ = ctx.write_single_register(0x00, password).await;
                *lock = Some(ctx);
            }
        }
    }

    // Subscribe to command topic
    let command_topic = format!("{}/{}/command/charge_battery", base_topic, inverter_name);
    if let Err(e) = mqtt_client
        .subscribe(&command_topic, QoS::AtLeastOnce)
        .await
    {
        println!(
            "Driver [{}] failed to subscribe to command topic: {}",
            inverter_name, e
        );
    }

    // Publish Home Assistant auto-discovery for charge_battery command
    crate::mqtt_helper::publish_home_assistant_discovery(
        &mqtt_client,
        &mqtt_config,
        &inverter_name,
        "charge_battery",
        true,
    )
    .await;

    // Spawn MQTT handler task
    let req_power_clone = requested_battery_power.clone();
    let inverter_name_clone = inverter_name.clone();
    let cancel_token_clone = cancel_token.clone();
    let mqtt_client_clone = mqtt_client.clone();
    let command_topic_clone = command_topic.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => break,
                res = eventloop.poll() => {
                    match res {
                        Ok(notification) => {
                            match notification {
                                Event::Incoming(Packet::ConnAck(_)) => {
                                    if let Err(e) = mqtt_client_clone
                                        .subscribe(&command_topic_clone, QoS::AtLeastOnce)
                                        .await
                                    {
                                        eprintln!(
                                            "Driver [{}] failed to re-subscribe to command topic on ConnAck: {}",
                                            inverter_name_clone, e
                                        );
                                    }
                                }
                                Event::Incoming(Packet::Publish(publish)) => {
                                    if publish.topic == command_topic_clone {
                                        let payload = String::from_utf8_lossy(&publish.payload);
                                        if let Ok(power) = payload.trim().parse::<i32>() {
                                            *req_power_clone.lock().await = power;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            println!(
                                "Driver [{}] MQTT eventloop error: {}",
                                inverter_name_clone, e
                            );
                            tokio::select! {
                                _ = cancel_token_clone.cancelled() => break,
                                _ = sleep(Duration::from_millis(500)) => {}
                            }
                        }
                    }
                }
            }
        }
    });

    // Main poll loop
    let mut power_budgets = VecDeque::new();
    let avg_samples = config.power_budget_avg_samples.unwrap_or(30);
    let poll_interval = Duration::from_secs_f64(config.poll_period);
    let mut discovered_metrics = std::collections::HashSet::new();
    let mut consecutive_errors = 0u32;
    let mut last_written_power: Option<i32> = None;
    let mut last_write_time: Option<std::time::Instant> = None;

    loop {
        if cancel_token.is_cancelled() {
            break;
        }
        let req_power = *requested_battery_power.lock().await;

        let read_res = {
            let mut lock = ctx_opt.lock().await;
            if lock.is_none() {
                match resolve_address(&hostname).await {
                    Ok(addr) => match tcp::connect(addr).await {
                        Ok(mut ctx) => {
                            let pwd = config.installer_password.unwrap_or(2014);
                            let _ = ctx.write_single_register(0x00, pwd).await;
                            tokio::time::sleep(Duration::from_millis(100)).await;

                            let _ = ctx.write_single_register(0x51, 1).await;
                            tokio::time::sleep(Duration::from_millis(100)).await;

                            *lock = Some(ctx);
                            consecutive_errors = 0;
                        }
                        Err(e) => {
                            println!("Driver [{}] failed to connect: {}", inverter_name, e);
                        }
                    },
                    Err(e) => {
                        println!("Driver [{}] failed to resolve: {}", inverter_name, e);
                    }
                }
            }
            if let Some(ref mut ctx) = *lock {
                while let Some((reg, val)) = crate::web_server::pop_pending_modbus_write(&inverter_name) {
                    let pwd = config.installer_password.unwrap_or(2014);
                    let _ = ctx.write_single_register(0x00, pwd).await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if let Err(e) = ctx.write_single_register(reg, val).await {
                        eprintln!("[SolaxModbus Log] Driver [{}] manual write ({:#06x}, {}) failed: {}", inverter_name, reg, val, e);
                    } else {
                        println!("[SolaxModbus Log] Driver [{}] manual write ({:#06x}, {}) succeeded", inverter_name, reg, val);
                    }
                }

                let needs_update = match (last_written_power, last_write_time) {
                    (Some(lp), Some(lt)) => lp != req_power || lt.elapsed() >= Duration::from_secs(10),
                    _ => true,
                };

                let mut write_failed = false;
                if needs_update {
                    last_written_power = Some(req_power);
                    last_write_time = Some(std::time::Instant::now());

                    // Gen 2 SK-SU Protocol: Write power target to register 0x0051 (positive = charge, negative = discharge)
                    // and trigger execution via register 0x0090 = 1
                    let power_u16 = (req_power as i16) as u16;
                    if let Err(e) = ctx.write_single_register(0x51, power_u16).await {
                        eprintln!("[SolaxModbus Log] Driver [{}] write_single_register(0x51, {}) failed: {}", inverter_name, req_power, e);
                        write_failed = true;
                    } else if req_power != 0 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let _ = ctx.write_single_register(0x90, 1).await;
                    }
                }

                let r_a = ctx.read_input_registers(0, 0x27).await;
                let r_b = ctx.read_input_registers(0x40, 0x1E).await;
                let r_h1 = ctx.read_holding_registers(0x0015, 17).await.ok();
                let r_h2 = ctx.read_holding_registers(0x008B, 23).await.ok();
                let r_h3 = ctx.read_holding_registers(0x00B4, 35).await.ok();

                if write_failed {
                    *lock = None;
                }

                match (r_a, r_b) {
                    (Ok(a), Ok(b)) => {
                        consecutive_errors = 0;
                        Ok((a, b, r_h1, r_h2, r_h3))
                    }
                    _ => {
                        consecutive_errors += 1;
                        if consecutive_errors >= 3 {
                            *lock = None; // Reset on 3 consecutive failures
                        }
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "Failed to read Modbus blocks",
                        ))
                    }
                }
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "Not connected",
                ))
            }
        };

        match read_res {
            Ok((reg_a, reg_b, r_h1, r_h2, r_h3)) => {
                if reg_a.len() >= 0x27 && reg_b.len() >= 0x1E {
                    let mut vals = parse_solax_registers(
                        &reg_a,
                        &reg_b,
                        req_power,
                        r_h1.as_deref(),
                        r_h2.as_deref(),
                        r_h3.as_deref(),
                    );

                    // Update global status for dashboard
                    let bat_cap = vals
                        .get("Battery Capacity")
                        .and_then(|v| v.parse::<u8>().ok())
                        .unwrap_or(0);
                    let bat_pow = vals
                        .get("Battery Power")
                        .and_then(|v| v.parse::<i32>().ok())
                        .unwrap_or(0);
                    let pv1 = vals
                        .get("PV1 Power")
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    let pv2 = vals
                        .get("PV2 Power")
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    let run_mode = vals
                        .get("Run Mode")
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);

                    let inv_fault = vals.get("Inverter Fault").and_then(|v| v.parse::<u32>().ok());
                    let chg_fault = vals.get("Charger Fault").and_then(|v| v.parse::<u32>().ok());
                    let mgr_fault = vals.get("Manager Fault").and_then(|v| v.parse::<u32>().ok());
                    let bms_warn = vals.get("BMS Warning").and_then(|v| v.parse::<u32>().ok());

                    let (is_err, err_text) = decipher_solax_v250_faults(run_mode, inv_fault, chg_fault, mgr_fault, bms_warn);

                    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                        let inv = status.inverters.entry(inverter_name.clone()).or_default();
                        inv.battery_capacity = bat_cap;
                        inv.battery_power = bat_pow;
                        inv.pv_power = pv1 + pv2;
                        inv.run_mode = run_mode;
                        inv.inverter_fault = inv_fault;
                        inv.charger_fault = chg_fault;
                        inv.manager_fault = mgr_fault;
                        inv.bms_warning = bms_warn;
                        inv.is_error = is_err;
                        inv.error_text = err_text;
                        inv.raw_metrics = vals.clone();
                        inv.driver_type = Some("Solax-Modbus".to_string());
                        inv.last_updated = Some(std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs());
                    }

                    // Add power budget average calculation
                    if let Some(pb_str) = vals.get("Power Budget") {
                        if let Ok(pb) = pb_str.parse::<i32>() {
                            power_budgets.push_back(pb);
                            if power_budgets.len() > avg_samples {
                                power_budgets.pop_front();
                            }
                            let avg: i32 =
                                power_budgets.iter().sum::<i32>() / power_budgets.len() as i32;
                            vals.insert("Power Budget Average".to_string(), avg.to_string());
                        }
                    }

                    let mut numeric_metrics = std::collections::HashMap::new();
                    for (metric, val_str) in &vals {
                        if let Ok(num) = val_str.parse::<f64>() {
                            numeric_metrics.insert(metric.clone(), num);
                        }
                    }
                    if !numeric_metrics.is_empty() {
                        let batch = crate::dispatch_manager::TelemetryBatch {
                            device_name: inverter_name.clone(),
                            timestamp: chrono::Utc::now().timestamp(),
                            metrics: numeric_metrics,
                        };
                        let _ = tx_telemetry.try_send(batch);
                    }

                    for (metric, val) in vals {
                        if !discovered_metrics.contains(&metric) {
                            crate::mqtt_helper::publish_home_assistant_discovery(
                                &mqtt_client,
                                &mqtt_config,
                                &inverter_name,
                                &metric,
                                false,
                            )
                            .await;
                            discovered_metrics.insert(metric.clone());
                        }

                        let topic = format!("{}/{}/{}", base_topic, inverter_name, metric);
                        let _ = mqtt_client
                            .publish(&topic, QoS::AtMostOnce, false, val)
                            .await;
                    }
                }
            }
            Err(e) => {
                println!("Driver [{}] failed to poll registers: {}", inverter_name, e);
            }
        }

        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = sleep(poll_interval) => {}
        }
    }
}

fn parse_solax_registers(
    reg_a: &[u16],
    reg_b: &[u16],
    requested_battery_power: i32,
    h1: Option<&[u16]>,
    h2: Option<&[u16]>,
    h3: Option<&[u16]>,
) -> HashMap<String, String> {
    let mut vals = HashMap::new();

    if let Some(r1) = h1 {
        if r1.len() >= 17 {
            vals.insert("PV Start Voltage".to_string(), format!("{:.1}", r1[0] as f64 * 0.1));
            vals.insert("Start Wait Time".to_string(), format!("{}", r1[1]));
            vals.insert("PV High Stop Voltage".to_string(), format!("{:.1}", r1[2] as f64 * 0.1));
            vals.insert("PV Low Stop Voltage".to_string(), format!("{:.1}", r1[3] as f64 * 0.1));
            vals.insert("Min Grid Voltage Protect".to_string(), format!("{:.1}", r1[4] as f64 * 0.1));
            vals.insert("Max Grid Voltage Protect".to_string(), format!("{:.1}", r1[5] as f64 * 0.1));
            vals.insert("Min Grid Freq Protect".to_string(), format!("{:.2}", r1[6] as f64 * 0.01));
            vals.insert("Max Grid Freq Protect".to_string(), format!("{:.2}", r1[7] as f64 * 0.01));
            vals.insert("Safety Type".to_string(), format!("{}", r1[8]));
            vals.insert("PV Connection Mode".to_string(), format!("{}", r1[9]));
            vals.insert("10Min Overvoltage Protect".to_string(), format!("{:.1}", r1[10] as f64 * 0.1));
            vals.insert("Min Slow Grid Voltage Protect".to_string(), format!("{:.1}", r1[11] as f64 * 0.1));
            vals.insert("Max Slow Grid Voltage Protect".to_string(), format!("{:.1}", r1[12] as f64 * 0.1));
            vals.insert("Min Slow Grid Freq Protect".to_string(), format!("{:.2}", r1[13] as f64 * 0.01));
            vals.insert("Max Slow Grid Freq Protect".to_string(), format!("{:.2}", r1[14] as f64 * 0.01));
            vals.insert("DCI Limit".to_string(), format!("{}", r1[15]));
            vals.insert("Active Power Limit".to_string(), format!("{}", r1[16]));
        }
    }

    if let Some(r2) = h2 {
        if r2.len() >= 23 {
            vals.insert("SolarChargerUseMode".to_string(), format!("{}", r2[0]));
            vals.insert("Work Mode".to_string(), format!("{}", r2[0]));
            vals.insert("Battery Min Capacity".to_string(), format!("{}", r2[1]));
            vals.insert("Battery Type".to_string(), format!("{}", r2[2]));
            vals.insert("Charge Cutoff Voltage".to_string(), format!("{:.2}", r2[3] as f64 * 0.01));
            vals.insert("Discharge Cutoff Voltage".to_string(), format!("{:.2}", r2[4] as f64 * 0.01));
            vals.insert("Max Charge Current".to_string(), format!("{:.2}", r2[5] as f64 * 0.01));
            vals.insert("Max Discharge Current".to_string(), format!("{:.2}", r2[6] as f64 * 0.01));

            vals.insert("Charge Window 1 Start".to_string(), format!("{:02}:{:02}", r2[7] & 0xFF, (r2[7] >> 8) & 0xFF));
            vals.insert("Charge Window 1 End".to_string(), format!("{:02}:{:02}", r2[9] & 0xFF, (r2[9] >> 8) & 0xFF));
            vals.insert("Discharge Window 1 Start".to_string(), format!("{:02}:{:02}", r2[11] & 0xFF, (r2[11] >> 8) & 0xFF));
            vals.insert("Discharge Window 1 End".to_string(), format!("{:02}:{:02}", r2[13] & 0xFF, (r2[13] >> 8) & 0xFF));
            vals.insert("Charge Window 2 Start".to_string(), format!("{:02}:{:02}", r2[15] & 0xFF, (r2[15] >> 8) & 0xFF));
            vals.insert("Charge Window 2 End".to_string(), format!("{:02}:{:02}", r2[17] & 0xFF, (r2[17] >> 8) & 0xFF));
            vals.insert("Discharge Window 2 Start".to_string(), format!("{:02}:{:02}", r2[19] & 0xFF, (r2[19] >> 8) & 0xFF));
            vals.insert("Discharge Window 2 End".to_string(), format!("{:02}:{:02}", r2[21] & 0xFF, (r2[21] >> 8) & 0xFF));
        }
    }

    if let Some(r3) = h3 {
        if r3.len() >= 9 {
            vals.insert("Allow Grid Charge".to_string(), format!("{}", r3[0]));
            vals.insert("Export Control Factory Limit".to_string(), format!("{}", r3[1]));
            vals.insert("Export Control User Limit".to_string(), format!("{}", r3[2]));
            vals.insert("Export Control Limit".to_string(), format!("{}", r3[2]));
            vals.insert("EPS Mute".to_string(), format!("{}", r3[3]));
            vals.insert("EPS Frequency".to_string(), format!("{}", r3[4]));
            vals.insert("EPS Discharge Voltage".to_string(), format!("{:.1}", r3[5] as f64 * 0.1));
            vals.insert("Language".to_string(), format!("{}", r3[7]));
            vals.insert("IP Method".to_string(), format!("{}", r3[8]));
            if r3.len() >= 35 {
                vals.insert("Charge Absorption Voltage".to_string(), format!("{:.2}", r3[34] as f64 * 0.01));
            }
        }
    }

    let unsigned16_a = |addr: usize| -> u32 { reg_a[addr] as u32 };
    let signed16_a = |addr: usize| -> i32 { reg_a[addr] as i16 as i32 };
    let unsigned32_a = |addr: usize| -> u32 {
        let low = reg_a[addr] as u32;
        let high = reg_a[addr + 1] as u32;
        low | (high << 16)
    };

    let unsigned16_b = |addr: usize| -> u32 { reg_b[addr - 0x40] as u32 };
    let _signed16_b = |addr: usize| -> i32 { reg_b[addr - 0x40] as i16 as i32 };
    let unsigned32_b = |addr: usize| -> u32 {
        let low = reg_b[addr - 0x40] as u32;
        let high = reg_b[addr + 1 - 0x40] as u32;
        low | (high << 16)
    };
    let signed32_b = |addr: usize| -> i32 { unsigned32_b(addr) as i32 };

    vals.insert(
        "Requested Battery Power".to_string(),
        requested_battery_power.to_string(),
    );
    vals.insert(
        "Grid Voltage".to_string(),
        format!("{:.1}", unsigned16_a(0x00) as f64 / 10.0),
    );
    vals.insert(
        "Grid Current".to_string(),
        format!("{:.1}", signed16_a(0x01) as f64 / 10.0),
    );
    vals.insert("Inverter Power".to_string(), signed16_a(0x02).to_string());
    vals.insert(
        "PV1 Voltage".to_string(),
        format!("{:.1}", unsigned16_a(0x03) as f64 / 10.0),
    );
    vals.insert(
        "PV2 Voltage".to_string(),
        format!("{:.1}", unsigned16_a(0x04) as f64 / 10.0),
    );
    vals.insert(
        "PV1 Current".to_string(),
        format!("{:.1}", unsigned16_a(0x05) as f64 / 10.0),
    );
    vals.insert(
        "PV2 Current".to_string(),
        format!("{:.1}", unsigned16_a(0x06) as f64 / 10.0),
    );
    vals.insert(
        "Grid Frequency".to_string(),
        format!("{:.2}", unsigned16_a(0x07) as f64 / 100.0),
    );
    vals.insert("Inner Temp".to_string(), signed16_a(0x08).to_string());
    vals.insert("Run Mode".to_string(), unsigned16_a(0x09).to_string());
    vals.insert("PV1 Power".to_string(), unsigned16_a(0x0a).to_string());
    vals.insert("PV2 Power".to_string(), unsigned16_a(0x0b).to_string());
    vals.insert(
        "Battery Voltage".to_string(),
        format!("{:.2}", signed16_a(0x14) as f64 / 100.0),
    );
    vals.insert(
        "Battery Current".to_string(),
        format!("{:.2}", signed16_a(0x15) as f64 / 100.0),
    );
    vals.insert("Battery Power".to_string(), (-signed16_a(0x16)).to_string());
    vals.insert(
        "Charger Board Temperature".to_string(),
        signed16_a(0x17).to_string(),
    );
    vals.insert(
        "Charger Battery Temperature".to_string(),
        signed16_a(0x18).to_string(),
    );
    vals.insert(
        "Charger Boost Temperature".to_string(),
        signed16_a(0x19).to_string(),
    );
    vals.insert("Battery Capacity".to_string(), unsigned16_a(0x1C).to_string());
    vals.insert(
        "Battery Energy Discharged".to_string(),
        format!("{:.1}", unsigned32_a(0x1D) as f64 / 10.0),
    );
    vals.insert("BMS Warning".to_string(), unsigned16_a(0x1F).to_string());
    vals.insert(
        "Battery Energy Charged".to_string(),
        format!("{:.1}", unsigned32_a(0x20) as f64 / 10.0),
    );
    vals.insert(
        "Battery State of Health".to_string(),
        unsigned16_a(0x23).to_string(),
    );
    vals.insert("Inverter Fault".to_string(), unsigned32_b(0x40).to_string());
    vals.insert("Charger Fault".to_string(), unsigned16_b(0x42).to_string());
    vals.insert("Manager Fault".to_string(), unsigned16_b(0x43).to_string());

    let measured_power = signed32_b(0x46);
    vals.insert("Measured Power".to_string(), measured_power.to_string());
    vals.insert(
        "Feed In Energy".to_string(),
        format!("{:.2}", unsigned32_b(0x48) as f64 / 100.0),
    );
    vals.insert(
        "Consumed Energy".to_string(),
        format!("{:.2}", unsigned32_b(0x4A) as f64 / 100.0),
    );
    vals.insert(
        "EPS Voltage".to_string(),
        format!("{:.1}", unsigned16_b(0x4C) as f64 / 10.0),
    );
    vals.insert(
        "EPS Current".to_string(),
        format!("{:.1}", unsigned16_b(0x4D) as f64 / 10.0),
    );
    vals.insert("EPS VA".to_string(), unsigned16_b(0x4E).to_string());
    vals.insert(
        "EPS Frequency".to_string(),
        format!("{:.2}", unsigned16_b(0x4F) as f64 / 100.0),
    );
    vals.insert(
        "Energy Today".to_string(),
        format!("{:.1}", unsigned16_b(0x50) as f64 / 10.0),
    );
    vals.insert(
        "Energy Total".to_string(),
        format!("{:.3}", unsigned32_b(0x52) as f64 / 1000.0),
    );
    vals.insert(
        "Battery Temperature".to_string(),
        format!("{:.1}", unsigned16_b(0x55) as f64 / 10.0),
    );

    let battery_power = signed16_a(0x16);
    let power_budget = battery_power + measured_power;
    vals.insert("Power Budget".to_string(), power_budget.to_string());

    let usage = signed16_a(0x02) - measured_power;
    vals.insert("Usage".to_string(), usage.to_string());

    vals
}

pub fn decipher_solax_v250_faults(
    run_mode: u32,
    inv_fault: Option<u32>,
    chg_fault: Option<u32>,
    mgr_fault: Option<u32>,
    bms_warning: Option<u32>,
) -> (bool, Option<String>) {
    let mut errors = Vec::new();

    match run_mode {
        3 => errors.push("Run Mode: Fault Mode".to_string()),
        4 => errors.push("Run Mode: Permanent Fault Mode".to_string()),
        _ => {}
    }

    if let Some(f) = inv_fault {
        if f > 0 {
            let labels = [
                "Hardware Trip", "Mains Lost", "Grid Voltage Fault", "Grid Frequency Fault",
                "PV Voltage Fault", "Bus Voltage Fault", "Battery Voltage Fault", "AC 10Mins Voltage Fault",
                "DCI OCP Fault", "DCV OCP Fault", "SW OCP Fault", "RC OCP Fault",
                "Isolation Fault", "Overtemperature Fault", "Bat Connection Dir Fault", "Sample Consistence Fault",
                "EPS Overload", "EPS OCP Fault", "Input Config Fault", "Firmware Ver Fault",
                "EPS Bat Power Low", "Phase Angle Fault", "PLL Overtime", "BMS Lost / Parallel Fault",
                "Inter-Com Fault", "Fan Fault", "HCT AC Device Fault", "EEPROM Fault",
                "Residual Current Device Fault", "EPS Relay Fault", "Grid Relay Fault", "Battery Relay Fault",
            ];
            for i in 0..32 {
                if (f & (1u32 << i)) != 0 {
                    if i < labels.len() {
                        errors.push(format!("Inverter Fault: {}", labels[i]));
                    } else {
                        errors.push(format!("Inverter Fault: Bit {}", i));
                    }
                }
            }
        }
    }

    if let Some(f) = chg_fault {
        if f > 0 {
            let labels = [
                "Boost Overcurrent", "Battery Overcurrent", "Battery Overvoltage", "Boost Overvoltage",
                "Charger Bus Overvoltage", "Charger Fan Fault", "Charger EPS Overload", "Charger High Temp Error",
                "Charger Low Temp Error", "Battery Awaken Failed", "Current Sensor Boost Fault", "Current Sensor Bat Fault",
                "Charger EEPROM WR Fault", "Charger Unrecoverable Fan Fault", "SPI Error", "Charger CAN Error",
            ];
            for i in 0..16 {
                if (f & (1u32 << i)) != 0 {
                    if i < labels.len() {
                        errors.push(format!("Charger Error: {}", labels[i]));
                    } else {
                        errors.push(format!("Charger Error: Bit {}", i));
                    }
                }
            }
        }
    }

    if let Some(f) = mgr_fault {
        if f > 0 {
            let labels = [
                "Internet IC Error (DM9000)", "RTC Real-Time Clock Error", "EEPROM IC Error",
                "Inter-communication Error (CAN)", "CT / Meter Connection Missing",
            ];
            for i in 0..5 {
                if (f & (1u32 << i)) != 0 {
                    errors.push(format!("Manager Error: {}", labels[i]));
                }
            }
        }
    }

    if let Some(f) = bms_warning {
        if f > 0 {
            let labels = [
                "BMS Disconnected", "BMS Alarm", "BMS Overvoltage", "BMS Undervoltage",
                "BMS Charge Overcurrent", "BMS Discharge Overcurrent", "BMS High Temp Warning", "BMS Low Temp Warning",
                "BMS Cell Imbalance",
            ];
            for i in 0..9 {
                if (f & (1u32 << i)) != 0 {
                    if i < labels.len() {
                        errors.push(format!("BMS Warning: {}", labels[i]));
                    } else {
                        errors.push(format!("BMS Warning: Bit {}", i));
                    }
                }
            }
        }
    }

    if !errors.is_empty() {
        (true, Some(errors.join(" | ")))
    } else {
        (false, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_solax_registers() {
        let mut reg_a = vec![0u16; 39];
        let mut reg_b = vec![0u16; 30];
        reg_a[0x00] = 2300; // Grid Voltage = 230.0V
        reg_a[0x01] = 105; // Grid Current = 10.5A
        reg_a[0x02] = 2000; // Inverter Power = 2000W
        reg_a[0x1C] = 15; // Battery Capacity = 15%
        reg_b[0x46 - 0x40] = 1000; // Measured Power = 1000W

        let parsed = parse_solax_registers(&reg_a, &reg_b, 500, None, None, None);
        assert_eq!(parsed.get("Grid Voltage").unwrap(), "230.0");
        assert_eq!(parsed.get("Grid Current").unwrap(), "10.5");
        assert_eq!(parsed.get("Inverter Power").unwrap(), "2000");
        assert_eq!(parsed.get("Battery Capacity").unwrap(), "15");
        assert_eq!(parsed.get("Measured Power").unwrap(), "1000");
        assert_eq!(parsed.get("Requested Battery Power").unwrap(), "500");
    }
}
