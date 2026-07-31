use crate::config::{MqttBrokerConfig, SolaxXHybridModbusConfig};
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

pub async fn run_solax_xhybrid_driver(
    inverter_name: String,
    hostname: String,
    config: SolaxXHybridModbusConfig,
    mqtt_config: MqttBrokerConfig,
    cancel_token: CancellationToken,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = format!("powerscraper-xhybrid-{}", inverter_name);
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    let ctx_opt: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
    let requested_battery_power = Arc::new(Mutex::new(0i32));

    // Handle handshake sequence for Hybrid
    let ctx_clone = ctx_opt.clone();
    let hostname_clone = hostname.clone();
    let password = config.installer_password;
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
            if let Some(pwd) = password {
                let _ = ctx.write_single_register(0x00, pwd).await;
            }
            let _ = ctx.write_single_register(0x9F, 30).await;
            let _ = ctx.write_single_register(0x51, 1).await;
            let _ = ctx.write_single_register(0x53, 0).await;
            let _ = ctx.write_single_register(0x40, 3).await;
        }
    });

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

    // Spawn MQTT command handler
    let ctx_clone = ctx_opt.clone();
    let hostname_clone = hostname.clone();
    let req_power_clone = requested_battery_power.clone();
    let inverter_name_clone = inverter_name.clone();
    let cancel_token_clone = cancel_token.clone();
    let mut last_power: Option<i32> = None;
    let mut last_write_time: Option<std::time::Instant> = None;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => break,
                res = eventloop.poll() => {
                    match res {
                        Ok(notification) => {
                            if let Event::Incoming(Packet::Publish(publish)) = notification {
                                if publish.topic == command_topic {
                                    let payload = String::from_utf8_lossy(&publish.payload);
                                    if let Ok(power) = payload.trim().parse::<i32>() {
                                        *req_power_clone.lock().await = power;

                                        let needs_update = match (last_power, last_write_time) {
                                            (Some(lp), Some(lt)) => lp != power || lt.elapsed() >= Duration::from_secs(10),
                                            _ => true,
                                        };

                                        if needs_update {
                                            if let Some(lt) = last_write_time {
                                                let elapsed = lt.elapsed();
                                                if elapsed < Duration::from_secs(1) {
                                                    sleep(Duration::from_secs(1) - elapsed).await;
                                                }
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
                                                // 1. Enable power control (0x0051)
                                                let _ = ctx.write_single_register(0x51, 1).await;
                                                // 2. Set keepalive timeout (0x009F)
                                                let _ = ctx.write_single_register(0x9F, 30).await;
                                                // 3. Write target power to Modbus ActivePower (0x0052)
                                                let power_u16 = power as u16;
                                                if ctx.write_single_register(0x52, power_u16).await.is_ok() {
                                                    last_power = Some(power);
                                                    last_write_time = Some(std::time::Instant::now());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!(
                                "Driver [{}] MQTT eventloop error: {}",
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

    // Main poll loop for XHybrid
    let mut power_budgets = VecDeque::new();
    let avg_samples = config.power_budget_avg_samples.unwrap_or(30);
    let poll_interval = Duration::from_secs_f64(config.poll_period);
    let mut discovered_metrics = std::collections::HashSet::new();

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
                        Ok(ctx) => {
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
            if let Some(ref mut ctx) = *lock {
                let r_a = ctx.read_input_registers(0, 0x27).await;
                let r_b = ctx.read_input_registers(0x40, 0x69 - 0x40 + 1).await;
                let r_c = ctx.read_input_registers(0x6A, 0xCD - 0x6A + 1).await;
                match (r_a, r_b, r_c) {
                    (Ok(a), Ok(b), Ok(c)) => Ok((a, b, c)),
                    (a, b, c) => {
                        *lock = None; // reset connection
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "Hybrid read fail: A={:?}, B={:?}, C={:?}",
                                a.err(),
                                b.err(),
                                c.err()
                            ),
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
            Ok((reg_a, reg_b, reg_c)) => {
                if reg_a.len() >= 0x27
                    && reg_b.len() >= (0x69 - 0x40 + 1)
                    && reg_c.len() >= (0xCD - 0x6A + 1)
                {
                    let mut vals = parse_hybrid_registers(&reg_a, &reg_b, &reg_c, req_power);

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

                    vals.insert("name".to_string(), inverter_name.clone());

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
    let _i32_c = |addr: usize| -> i32 { u32_c(addr) as i32 };

    vals.insert(
        "Requested Battery Power".to_string(),
        requested_battery_power.to_string(),
    );
    vals.insert(
        "Grid Voltage (X1)".to_string(),
        format!("{:.1}", u16_a(0x00) as f64 / 10.0),
    );
    vals.insert(
        "Grid Current (X1)".to_string(),
        format!("{:.1}", i16_a(0x01) as f64 / 10.0),
    );
    vals.insert("Inverter Power (X1)".to_string(), i16_a(0x02).to_string());
    vals.insert(
        "PV1 Voltage (Hybrid)".to_string(),
        format!("{:.1}", u16_a(0x03) as f64 / 10.0),
    );
    vals.insert(
        "PV2 Voltage (Hybrid)".to_string(),
        format!("{:.1}", u16_a(0x04) as f64 / 10.0),
    );
    vals.insert(
        "PV1 Current (Hybrid)".to_string(),
        format!("{:.1}", u16_a(0x05) as f64 / 10.0),
    );
    vals.insert(
        "PV2 Current (Hybrid)".to_string(),
        format!("{:.1}", u16_a(0x06) as f64 / 10.0),
    );
    vals.insert(
        "Grid Frequency (X1)".to_string(),
        format!("{:.2}", u16_a(0x07) as f64 / 100.0),
    );
    vals.insert("Inner Temp".to_string(), i16_a(0x08).to_string());
    vals.insert("Run Mode".to_string(), u16_a(0x09).to_string());
    vals.insert("PV1 Power".to_string(), u16_a(0x0a).to_string());
    vals.insert("PV2 Power".to_string(), u16_a(0x0b).to_string());
    vals.insert(
        "Battery Voltage".to_string(),
        format!("{:.2}", i16_a(0x14) as f64 / 100.0),
    );
    vals.insert(
        "Battery Current".to_string(),
        format!("{:.2}", i16_a(0x15) as f64 / 100.0),
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

    // Block B
    vals.insert("Inverter Fault".to_string(), u32_b(0x40).to_string());
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
    vals.insert(
        "Battery Temperature".to_string(),
        format!("{:.1}", u16_b(0x55) as f64 / 10.0),
    );
    vals.insert(
        "Solar Energy Total".to_string(),
        format!("{:.1}", u32_b(0x56) as f64 / 10.0),
    );
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

    // Block C
    let battery_power = i16_c(0xC4);
    let measured_power_x3_p1 = i16_c(0xBE);
    let measured_power_x3_p2 = i16_c(0xBF);
    let measured_power_x3_p3 = i16_c(0xC0);
    vals.insert("Grid Power (P1)".to_string(), measured_power_x3_p1.to_string());
    vals.insert("Grid Power (P2)".to_string(), measured_power_x3_p2.to_string());
    vals.insert("Grid Power (P3)".to_string(), measured_power_x3_p3.to_string());
    vals.insert("Battery Temperature (Hybrid)".to_string(), i16_c(0xC6).to_string());
    vals.insert("Inner Temp (Hybrid)".to_string(), i16_c(0xC8).to_string());
    vals.insert("Inverter Power (X3)".to_string(), i16_c(0xC5).to_string());
    vals.insert("BMS Error".to_string(), u16_c(0xCA).to_string());
    vals.insert(
        "BMS Warning (Hybrid)".to_string(),
        u16_c(0xCB).to_string(),
    );
    vals.insert("BMS Energy Throughput".to_string(), u32_c(0xCC).to_string());
    vals.insert("Run Mode 2".to_string(), u16_c(0xBF).to_string());
    vals.insert("BMS Connected".to_string(), u16_c(0xC5).to_string());

    let power_budget = battery_power
        + measured_power
        + measured_power_x3_p1
        + measured_power_x3_p2
        + measured_power_x3_p3;
    vals.insert("Power Budget".to_string(), power_budget.to_string());

    let grid_power = i16_c(0xBC);
    let usage = grid_power - measured_power;
    vals.insert("Usage".to_string(), usage.to_string());

    vals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hybrid_registers() {
        let reg_a = vec![0u16; 0x27];
        let reg_b = vec![0u16; 0x69 - 0x40 + 1];
        let reg_c = vec![0u16; 0xCD - 0x6A + 1];

        let parsed = parse_hybrid_registers(&reg_a, &reg_b, &reg_c, 100);
        assert_eq!(parsed.get("Requested Battery Power").unwrap(), "100");
    }
}
