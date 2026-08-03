use crate::config::{MqttBrokerConfig, SolaxG3ModbusConfig};
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

pub async fn run_solax_g3_driver(
    inverter_name: String,
    hostname: String,
    config: SolaxG3ModbusConfig,
    mqtt_config: MqttBrokerConfig,
    tx_telemetry: tokio::sync::mpsc::Sender<crate::dispatch_manager::TelemetryBatch>,
    cancel_token: CancellationToken,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = format!("powerscraper-g3-{}", inverter_name);
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    let ctx_opt: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
    let requested_battery_power = Arc::new(Mutex::new(0i32));

    let command_topic = format!("{}/{}/command/charge_battery", base_topic, inverter_name);

    // Spawn MQTT eventloop handler task first so poll() processes Network I/O
    let req_power_clone = requested_battery_power.clone();
    let inverter_name_clone = inverter_name.clone();
    let cancel_token_clone = cancel_token.clone();
    let command_topic_clone = command_topic.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => break,
                res = eventloop.poll() => {
                    match res {
                        Ok(notification) => {
                            if let Event::Incoming(Packet::Publish(publish)) = notification {
                                if publish.topic == command_topic_clone {
                                    let payload = String::from_utf8_lossy(&publish.payload);
                                    if let Ok(power) = payload.trim().parse::<i32>() {
                                        *req_power_clone.lock().await = power;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[SolaxG3 Log] Driver [{}] MQTT eventloop error: {}",
                                inverter_name_clone, e
                            );
                            tokio::select! {
                                _ = cancel_token_clone.cancelled() => break,
                                _ = sleep(Duration::from_secs(5)) => {}
                            }
                        }
                    }
                }
            }
        }
    });

    // Seed status.inverters so the inverter appears in web UI immediately
    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
        let inv = status.inverters.entry(inverter_name.clone()).or_default();
        inv.run_mode = 0;
    }

    // Handle handshake sequence for Hybrid
    let ctx_clone = ctx_opt.clone();
    let hostname_clone = hostname.clone();
    let cancel_token_handshake = cancel_token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel_token_handshake.cancelled() => return,
            _ = sleep(Duration::from_secs(2)) => {}
        }
        let mut lock = ctx_clone.lock().await;
        if lock.is_none() {
            if let Ok(addr) = resolve_address(&hostname_clone).await {
                if let Ok(ctx) = tcp::connect(addr).await {
                    *lock = Some(ctx);
                }
            }
        }
        if let Some(ref mut ctx) = *lock {
            let _ = ctx.write_single_register(0x9F, 30).await;
            let _ = ctx.write_single_register(0x51, 1).await;
            let _ = ctx.write_single_register(0x53, 0).await;
            let _ = ctx.write_single_register(0x40, 3).await;
        }
    });

    // Subscribe to command topic
    if let Err(e) = mqtt_client
        .subscribe(&command_topic, QoS::AtLeastOnce)
        .await
    {
        eprintln!(
            "[SolaxG3 Log] Driver [{}] failed to subscribe to command topic: {}",
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

    // Main poll loop for G3
    let mut power_budgets = VecDeque::new();
    let avg_samples = config.power_budget_avg_samples.unwrap_or(30);
    let poll_interval = Duration::from_secs_f64(config.poll_period);
    let mut discovered_metrics = std::collections::HashSet::new();
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

                            let _ = ctx.write_single_register(0x9F, 30).await;
                            tokio::time::sleep(Duration::from_millis(100)).await;

                            *lock = Some(ctx);
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
            let mut reset_connection = false;
            let mut read_data = None;

            if let Some(ref mut ctx) = *lock {
                let needs_update = match (last_written_power, last_write_time) {
                    (Some(lp), Some(lt)) => lp != req_power || lt.elapsed() >= Duration::from_secs(10),
                    _ => true,
                };

                if needs_update {
                    last_written_power = Some(req_power);
                    last_write_time = Some(std::time::Instant::now());

                    // Write target power to Modbus ActivePower (0x0052)
                    // Sign convention: positive req_power = Charge, negative req_power = Discharge
                    let power_u16 = (req_power as i16) as u16;
                    if let Err(e) = ctx.write_single_register(0x52, power_u16).await {
                        eprintln!("[SolaxG3 Log] Driver [{}] write_single_register(0x52, {}) failed: {}", inverter_name, req_power, e);
                        reset_connection = true;
                    }
                }

                if !reset_connection {
                    // Split queries into smaller non-contiguous blocks to avoid Modbus address exceptions
                    let r_a = ctx.read_input_registers(0, 0x29).await;
                    let r_b1 = ctx.read_input_registers(0x40, 20).await;
                    let r_b2 = ctx.read_input_registers(0x66, 4).await;
                    let r_c1 = ctx.read_input_registers(0x6A, 0x0C).await;
                    let r_c2 = ctx.read_input_registers(0xBC, 18).await;

                    if let Ok(a) = r_a {
                        let mut b = vec![0u16; 42];
                        if let Ok(ref b1) = r_b1 {
                            let len = b1.len().min(30);
                            b[0..len].copy_from_slice(&b1[0..len]);
                        }
                        if let Ok(ref b2) = r_b2 {
                            let len = b2.len().min(4);
                            b[38..38 + len].copy_from_slice(&b2[0..len]);
                        }
                        let mut c = vec![0u16; 100];
                        if let Ok(ref c1) = r_c1 {
                            let len = c1.len().min(12);
                            c[0..len].copy_from_slice(&c1[0..len]);
                        }
                        if let Ok(ref c2) = r_c2 {
                            let len = c2.len().min(18);
                            let start = 0xBC - 0x6A; // 82
                            c[start..start + len].copy_from_slice(&c2[0..len]);
                        }
                        read_data = Some((a, b, c));
                    } else {
                        println!(
                            "Driver [{}] Modbus read fail: r_a={:?}",
                            inverter_name,
                            r_a.as_ref().err()
                        );
                        reset_connection = true;
                    }
                }
            }

            if reset_connection {
                *lock = None;
            }

            if let Some((a, b, c)) = read_data {
                Ok((a, b, c))
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "Failed to read core Modbus blocks",
                ))
            }
        };

        match read_res {
            Ok((reg_a, reg_b, reg_c)) => {
                if reg_a.len() >= 0x1D {
                    let mut vals = parse_hybrid_registers(&reg_a, &reg_b, &reg_c, req_power);
                    eprintln!(
                        "[SolaxG3 Log] Driver [{}] parsed {} metrics",
                        inverter_name,
                        vals.len()
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

                    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                        let inv = status.inverters.entry(inverter_name.clone()).or_default();
                        inv.battery_capacity = bat_cap;
                        inv.battery_power = bat_pow;
                        inv.pv_power = pv1 + pv2;
                        inv.run_mode = run_mode;
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
                println!("Driver [{}] failed to poll: {}", inverter_name, e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }

        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = sleep(poll_interval) => {}
        }
    }
}

fn parse_hybrid_registers(
    reg_a: &[u16],
    reg_b: &[u16],
    reg_c: &[u16],
    requested_battery_power: i32,
) -> HashMap<String, String> {
    let mut vals = HashMap::new();

    let u16_a = |addr: usize| -> u32 { reg_a[addr] as u32 };
    let i16_a = |addr: usize| -> i32 { reg_a[addr] as i16 as i32 };
    let u32_a_split = |addr_l: usize, addr_h: usize| -> u32 {
        let low = reg_a[addr_l] as u32;
        let high = reg_a[addr_h] as u32;
        low | (high << 16)
    };
    let u32_a = |addr: usize| -> u32 { u32_a_split(addr, addr + 1) };

    let u16_b = |addr: usize| -> u32 { reg_b[addr - 0x40] as u32 };
    let u32_b = |addr: usize| -> u32 {
        let low = reg_b[addr - 0x40] as u32;
        let high = reg_b[addr + 1 - 0x40] as u32;
        low | (high << 16)
    };
    let i32_b = |addr: usize| -> i32 { u32_b(addr) as i32 };

    let u16_c = |addr: usize| -> u32 { reg_c[addr - 0x6A] as u32 };
    let i16_c = |addr: usize| -> i32 { reg_c[addr - 0x6A] as i16 as i32 };
    let u32_c = |addr: usize| -> u32 {
        let low = reg_c[addr - 0x6A] as u32;
        let high = reg_c[addr + 1 - 0x6A] as u32;
        low | (high << 16)
    };

    vals.insert(
        "Requested Battery Power".to_string(),
        requested_battery_power.to_string(),
    );
    let grid_v = format!("{:.1}", u16_a(0x00) as f64 / 10.0);
    vals.insert("Grid Voltage".to_string(), grid_v.clone());
    vals.insert("Grid Voltage X1".to_string(), grid_v);

    let grid_c = format!("{:.1}", i16_a(0x01) as f64 / 10.0);
    vals.insert("Grid Current".to_string(), grid_c.clone());
    vals.insert("Grid Current X1".to_string(), grid_c);

    let inv_p = i16_a(0x02).to_string();
    vals.insert("Inverter Power".to_string(), inv_p.clone());
    vals.insert("Inverter Power X1".to_string(), inv_p);

    let pv1_v = format!("{:.1}", u16_a(0x03) as f64 / 10.0);
    vals.insert("PV1 Voltage".to_string(), pv1_v.clone());
    vals.insert("PV1 Voltage Hybrid".to_string(), pv1_v);

    let pv2_v = format!("{:.1}", u16_a(0x04) as f64 / 10.0);
    vals.insert("PV2 Voltage".to_string(), pv2_v.clone());
    vals.insert("PV2 Voltage Hybrid".to_string(), pv2_v);

    let pv1_c = format!("{:.1}", u16_a(0x05) as f64 / 10.0);
    vals.insert("PV1 Current".to_string(), pv1_c.clone());
    vals.insert("PV1 Current Hybrid".to_string(), pv1_c);

    let pv2_c = format!("{:.1}", u16_a(0x06) as f64 / 10.0);
    vals.insert("PV2 Current".to_string(), pv2_c.clone());
    vals.insert("PV2 Current Hybrid".to_string(), pv2_c);

    let grid_f = format!("{:.2}", u16_a(0x07) as f64 / 100.0);
    vals.insert("Grid Frequency".to_string(), grid_f.clone());
    vals.insert("Grid Frequency X1".to_string(), grid_f);
    vals.insert("Inner Temp".to_string(), i16_a(0x08).to_string());
    vals.insert("Run Mode".to_string(), u16_a(0x09).to_string());
    vals.insert("PV1 Power".to_string(), u16_a(0x0a).to_string());
    vals.insert("PV2 Power".to_string(), u16_a(0x0b).to_string());
    vals.insert(
        "Battery Voltage".to_string(),
        format!("{:.1}", i16_a(0x14) as f64 / 10.0),
    );
    vals.insert(
        "Battery Current".to_string(),
        format!("{:.1}", i16_a(0x15) as f64 / 10.0),
    );
    vals.insert("Battery Power".to_string(), (-i16_a(0x16)).to_string());
    vals.insert("BMS Connect State".to_string(), u16_a(0x17).to_string());
    vals.insert("Battery Temperature".to_string(), i16_a(0x18).to_string());
    vals.insert(
        "Charger Boost Temperature".to_string(),
        i16_a(0x19).to_string(),
    );
    vals.insert("Battery Capacity".to_string(), u16_a(0x1C).to_string());
    vals.insert(
        "Battery Energy Discharged".to_string(),
        format!("{:.1}", u32_a(0x1D) as f64 / 10.0),
    );
    vals.insert(
        "BMS Warning".to_string(),
        u32_a_split(0x1F, 0x26).to_string(),
    );
    vals.insert(
        "Battery Energy Discharged Today".to_string(),
        u16_a(0x20).to_string(),
    );
    vals.insert(
        "Battery Energy Charged".to_string(),
        format!("{:.1}", u32_a(0x21) as f64 / 10.0),
    );
    vals.insert(
        "Battery Energy Charged Today".to_string(),
        u16_a(0x23).to_string(),
    );
    vals.insert(
        "BMS Max Charge Current".to_string(),
        format!("{:.1}", u16_a(0x24) as f64 / 10.0),
    );
    vals.insert(
        "BMS Max Discharge Current".to_string(),
        format!("{:.1}", u16_a(0x25) as f64 / 10.0),
    );
    if reg_a.len() >= 0x29 {
        vals.insert(
            "Battery State of Health".to_string(),
            u16_a(0x28).to_string(),
        );
    }

    // Block B
    vals.insert("Inverter Fault".to_string(), u32_b(0x40).to_string());
    vals.insert("Charger Fault".to_string(), u16_b(0x42).to_string());
    vals.insert("Manager Fault".to_string(), u16_b(0x43).to_string());
    let measured_power = i32_b(0x46);
    vals.insert("Measured Power".to_string(), measured_power.to_string());
    vals.insert(
        "Feed In Energy".to_string(),
        format!("{:.2}", u32_b(0x48) as f64 / 100.0),
    );
    vals.insert(
        "Consumed Energy".to_string(),
        format!("{:.2}", u32_b(0x4A) as f64 / 100.0),
    );
    vals.insert(
        "EPS Voltage".to_string(),
        format!("{:.1}", u16_b(0x4C) as f64 / 10.0),
    );
    vals.insert(
        "EPS Current".to_string(),
        format!("{:.1}", u16_b(0x4D) as f64 / 10.0),
    );
    vals.insert("EPS VA".to_string(), u16_b(0x4E).to_string());
    vals.insert(
        "EPS Frequency".to_string(),
        format!("{:.2}", u16_b(0x4F) as f64 / 100.0),
    );
    vals.insert(
        "Energy Today".to_string(),
        format!("{:.1}", u16_b(0x50) as f64 / 10.0),
    );
    vals.insert(
        "Energy Total".to_string(),
        format!("{:.3}", u32_b(0x52) as f64 / 1000.0),
    );
    if reg_b.len() >= (0x69 - 0x40 + 1) {
        vals.insert(
            "Bus Voltage".to_string(),
            format!("{:.1}", u16_b(0x66) as f64 / 10.0),
        );
        vals.insert(
            "DC Voltage Fault".to_string(),
            format!("{:.1}", u16_b(0x67) as f64 / 10.0),
        );
        vals.insert("Overload Fault".to_string(), u16_b(0x68).to_string());
        vals.insert("Battery Voltage Fault".to_string(), u16_b(0x69).to_string());
    } else if reg_b.len() >= 39 {
        vals.insert(
            "Bus Voltage".to_string(),
            format!("{:.1}", u16_b(0x66) as f64 / 10.0),
        );
    }

    // Block C: Three Phase Grid Telemetry & BMS (0x6A..0xCD)
    if reg_c.len() >= 0x0C {
        vals.insert(
            "Phase 1 Grid Voltage".to_string(),
            format!("{:.1}", u16_c(0x6A) as f64 / 10.0),
        );
        vals.insert(
            "Phase 1 Grid Current".to_string(),
            format!("{:.1}", i16_c(0x6B) as f64 / 10.0),
        );
        vals.insert(
            "Phase 1 Grid Power".to_string(),
            i16_c(0x6C).to_string(),
        );
        vals.insert(
            "Phase 2 Grid Voltage".to_string(),
            format!("{:.1}", u16_c(0x6E) as f64 / 10.0),
        );
        vals.insert(
            "Phase 2 Grid Current".to_string(),
            format!("{:.1}", i16_c(0x6F) as f64 / 10.0),
        );
        vals.insert(
            "Phase 2 Grid Power".to_string(),
            i16_c(0x70).to_string(),
        );
        vals.insert(
            "Phase 3 Grid Voltage".to_string(),
            format!("{:.1}", u16_c(0x72) as f64 / 10.0),
        );
        vals.insert(
            "Phase 3 Grid Current".to_string(),
            format!("{:.1}", i16_c(0x73) as f64 / 10.0),
        );
        vals.insert(
            "Phase 3 Grid Power".to_string(),
            i16_c(0x74).to_string(),
        );
    }

    if reg_c.len() >= 96 {
        let battery_power_c = i16_c(0xC4);
        let measured_power_x3_p1 = i16_c(0xBE);
        let measured_power_x3_p2 = i16_c(0xBF);
        let measured_power_x3_p3 = i16_c(0xC0);
        vals.insert("Grid Power (P1)".to_string(), measured_power_x3_p1.to_string());
        vals.insert("Grid Power (P2)".to_string(), measured_power_x3_p2.to_string());
        vals.insert("Grid Power (P3)".to_string(), measured_power_x3_p3.to_string());
        vals.insert("Battery Temperature (Hybrid)".to_string(), i16_c(0xC6).to_string());
        vals.insert("Inner Temp (Hybrid)".to_string(), i16_c(0xC8).to_string());
        vals.insert("Inverter Power (X3)".to_string(), i16_c(0xC5).to_string());
        vals.insert("Run Mode 2".to_string(), u16_c(0xBF).to_string());
        vals.insert("BMS Connected".to_string(), u16_c(0xC5).to_string());

        if reg_c.len() >= (0xCD - 0x6A + 1) {
            vals.insert("BMS Error".to_string(), u16_c(0xCA).to_string());
            vals.insert("BMS Warning (Hybrid)".to_string(), u16_c(0xCB).to_string());
            vals.insert("BMS Energy Throughput".to_string(), u32_c(0xCC).to_string());
        }

        let power_budget = battery_power_c + measured_power + measured_power_x3_p1 + measured_power_x3_p2 + measured_power_x3_p3;
        vals.insert("Power Budget".to_string(), power_budget.to_string());

        let grid_power = i16_c(0xBC);
        let usage = grid_power - measured_power;
        vals.insert("Usage".to_string(), usage.to_string());
    } else {
        let battery_power = i16_a(0x16);
        let power_budget = battery_power + measured_power;
        vals.insert("Power Budget".to_string(), power_budget.to_string());

        let usage = i16_a(0x02) - measured_power;
        vals.insert("Usage".to_string(), usage.to_string());
    }

    vals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_solax_g3_registers() {
        let mut reg_a = vec![0u16; 41];
        let mut reg_b = vec![0u16; 30];
        let mut reg_c = vec![0u16; 14];

        reg_a[0x00] = 2300; // Grid Voltage = 230.0V
        reg_a[0x01] = 105; // Grid Current = 10.5A
        reg_a[0x02] = 2000; // Inverter Power = 2000W
        reg_a[0x1C] = 15; // Battery Capacity = 15%
        reg_a[0x28] = 99; // Battery State of Health = 99%
        reg_b[0x46 - 0x40] = 1000; // Measured Power = 1000W

        // 3-Phase Grid Telemetry (R=0x6A..0x6D, S=0x6E..0x71, T=0x72..0x75)
        reg_c[0x6A - 0x6A] = 2310; // Phase 1 Voltage = 231.0V
        reg_c[0x6B - 0x6A] = 50;   // Phase 1 Current = 5.0A
        reg_c[0x6C - 0x6A] = 1155; // Phase 1 Power = 1155W
        reg_c[0x6E - 0x6A] = 2295; // Phase 2 Voltage = 229.5V
        reg_c[0x6F - 0x6A] = 48;   // Phase 2 Current = 4.8A
        reg_c[0x70 - 0x6A] = 1100; // Phase 2 Power = 1100W
        reg_c[0x72 - 0x6A] = 2305; // Phase 3 Voltage = 230.5V
        reg_c[0x73 - 0x6A] = 52;   // Phase 3 Current = 5.2A
        reg_c[0x74 - 0x6A] = 1198; // Phase 3 Power = 1198W

        let parsed = parse_hybrid_registers(&reg_a, &reg_b, &reg_c, 500);
        assert_eq!(parsed.get("Grid Voltage").unwrap(), "230.0");
        assert_eq!(parsed.get("Grid Current").unwrap(), "10.5");
        assert_eq!(parsed.get("Inverter Power").unwrap(), "2000");
        assert_eq!(parsed.get("Battery Capacity").unwrap(), "15");
        assert_eq!(parsed.get("Battery State of Health").unwrap(), "99");
        assert_eq!(parsed.get("Measured Power").unwrap(), "1000");
        assert_eq!(parsed.get("Requested Battery Power").unwrap(), "500");
        assert_eq!(parsed.get("Phase 1 Grid Voltage").unwrap(), "231.0");
        assert_eq!(parsed.get("Phase 1 Grid Current").unwrap(), "5.0");
        assert_eq!(parsed.get("Phase 1 Grid Power").unwrap(), "1155");
        assert_eq!(parsed.get("Phase 2 Grid Voltage").unwrap(), "229.5");
        assert_eq!(parsed.get("Phase 2 Grid Current").unwrap(), "4.8");
        assert_eq!(parsed.get("Phase 2 Grid Power").unwrap(), "1100");
        assert_eq!(parsed.get("Phase 3 Grid Voltage").unwrap(), "230.5");
        assert_eq!(parsed.get("Phase 3 Grid Current").unwrap(), "5.2");
        assert_eq!(parsed.get("Phase 3 Grid Power").unwrap(), "1198");
    }
}
