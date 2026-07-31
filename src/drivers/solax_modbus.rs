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

                                            let power_u16 = power as u16;
                                            let mut lock = ctx_clone.lock().await;
                                            if lock.is_none() {
                                                if let Ok(addr) = resolve_address(&hostname_clone).await {
                                                    if let Ok(ctx) = tcp::connect(addr).await {
                                                        *lock = Some(ctx);
                                                    }
                                                }
                                            }
                                            if let Some(ref mut ctx) = *lock {
                                                if ctx.write_single_register(0x51, power_u16).await.is_ok() {
                                                    last_power = Some(power);
                                                    last_write_time = Some(std::time::Instant::now());
                                                    if power != 0 {
                                                        let _ = ctx.write_single_register(0x90, 1).await;
                                                    }
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

    // Main poll loop
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
                match ctx.read_input_registers(0, 0x72).await {
                    Ok(regs) => Ok(regs),
                    Err(e) => {
                        *lock = None; // Reset on failure
                        Err(e)
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
            Ok(registers) => {
                if registers.len() >= 0x72 {
                    let mut vals = parse_solax_registers(&registers, req_power);

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
    registers: &[u16],
    requested_battery_power: i32,
) -> HashMap<String, String> {
    let mut vals = HashMap::new();

    let unsigned16 = |addr: usize| -> u32 { registers[addr] as u32 };

    let signed16 = |addr: usize| -> i32 { registers[addr] as i16 as i32 };

    let unsigned32 = |addr: usize| -> u32 {
        let low = registers[addr] as u32;
        let high = registers[addr + 1] as u32;
        low | (high << 16)
    };

    let signed32 = |addr: usize| -> i32 { unsigned32(addr) as i32 };

    vals.insert(
        "Requested Battery Power".to_string(),
        requested_battery_power.to_string(),
    );
    vals.insert(
        "Grid Voltage".to_string(),
        format!("{:.1}", unsigned16(0x00) as f64 / 10.0),
    );
    vals.insert(
        "Grid Current".to_string(),
        format!("{:.1}", signed16(0x01) as f64 / 10.0),
    );
    vals.insert("Inverter Power".to_string(), signed16(0x02).to_string());
    vals.insert(
        "PV1 Voltage".to_string(),
        format!("{:.1}", unsigned16(0x03) as f64 / 10.0),
    );
    vals.insert(
        "PV2 Voltage".to_string(),
        format!("{:.1}", unsigned16(0x04) as f64 / 10.0),
    );
    vals.insert(
        "PV1 Current".to_string(),
        format!("{:.1}", unsigned16(0x05) as f64 / 10.0),
    );
    vals.insert(
        "PV2 Current".to_string(),
        format!("{:.1}", unsigned16(0x06) as f64 / 10.0),
    );
    vals.insert(
        "Grid Frequency".to_string(),
        format!("{:.2}", unsigned16(0x07) as f64 / 100.0),
    );
    vals.insert("Inner Temp".to_string(), signed16(0x08).to_string());
    vals.insert("Run Mode".to_string(), unsigned16(0x09).to_string());
    vals.insert("PV1 Power".to_string(), unsigned16(0x0a).to_string());
    vals.insert("PV2 Power".to_string(), unsigned16(0x0b).to_string());
    vals.insert(
        "Battery Voltage".to_string(),
        format!("{:.2}", signed16(0x14) as f64 / 100.0),
    );
    vals.insert(
        "Battery Current".to_string(),
        format!("{:.2}", signed16(0x15) as f64 / 100.0),
    );
    vals.insert("Battery Power".to_string(), (-signed16(0x16)).to_string());
    vals.insert(
        "Charger Board Temperature".to_string(),
        signed16(0x17).to_string(),
    );
    vals.insert(
        "Charger Battery Temperature".to_string(),
        signed16(0x18).to_string(),
    );
    vals.insert(
        "Charger Boost Temperature".to_string(),
        signed16(0x19).to_string(),
    );
    vals.insert("Battery Capacity".to_string(), unsigned16(0x1C).to_string());
    vals.insert(
        "Battery Energy Discharged".to_string(),
        format!("{:.1}", unsigned32(0x1D) as f64 / 10.0),
    );
    vals.insert("BMS Warning".to_string(), unsigned16(0x1F).to_string());
    vals.insert(
        "Battery Energy Charged".to_string(),
        format!("{:.1}", unsigned32(0x20) as f64 / 10.0),
    );
    vals.insert(
        "Battery State of Health".to_string(),
        unsigned16(0x23).to_string(),
    );
    vals.insert("Inverter Fault".to_string(), unsigned32(0x40).to_string());
    vals.insert("Charger Fault".to_string(), unsigned16(0x42).to_string());
    vals.insert("Manager Fault".to_string(), unsigned16(0x43).to_string());

    let measured_power = signed32(0x46);
    vals.insert("Measured Power".to_string(), measured_power.to_string());
    vals.insert(
        "Feed In Energy".to_string(),
        format!("{:.2}", unsigned32(0x48) as f64 / 100.0),
    );
    vals.insert(
        "Consumed Energy".to_string(),
        format!("{:.2}", unsigned32(0x4A) as f64 / 100.0),
    );
    vals.insert(
        "EPS Voltage".to_string(),
        format!("{:.1}", unsigned16(0x4C) as f64 / 10.0),
    );
    vals.insert(
        "EPS Current".to_string(),
        format!("{:.1}", unsigned16(0x4D) as f64 / 10.0),
    );
    vals.insert("EPS VA".to_string(), unsigned16(0x4E).to_string());
    vals.insert(
        "EPS Frequency".to_string(),
        format!("{:.2}", unsigned16(0x4F) as f64 / 100.0),
    );
    vals.insert(
        "Energy Today".to_string(),
        format!("{:.1}", unsigned16(0x50) as f64 / 10.0),
    );
    vals.insert(
        "Energy Total".to_string(),
        format!("{:.3}", unsigned32(0x52) as f64 / 1000.0),
    );
    vals.insert(
        "Battery Temperature".to_string(),
        format!("{:.1}", unsigned16(0x55) as f64 / 10.0),
    );
    vals.insert(
        "Solar Energy Total".to_string(),
        format!("{:.1}", unsigned32(0x70) as f64 / 10.0),
    );

    let battery_power = signed16(0x16);
    let power_budget = battery_power + measured_power;
    vals.insert("Power Budget".to_string(), power_budget.to_string());

    let usage = signed16(0x02) - measured_power;
    vals.insert("Usage".to_string(), usage.to_string());

    vals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_solax_registers() {
        let mut regs = vec![0u16; 114];
        regs[0x00] = 2300; // Grid Voltage = 230.0V
        regs[0x01] = 105; // Grid Current = 10.5A
        regs[0x02] = 2000; // Inverter Power = 2000W
        regs[0x1C] = 15; // Battery Capacity = 15%
        regs[0x46] = 1000; // Measured Power = 1000W

        let parsed = parse_solax_registers(&regs, 500);
        assert_eq!(parsed.get("Grid Voltage").unwrap(), "230.0");
        assert_eq!(parsed.get("Grid Current").unwrap(), "10.5");
        assert_eq!(parsed.get("Inverter Power").unwrap(), "2000");
        assert_eq!(parsed.get("Battery Capacity").unwrap(), "15");
        assert_eq!(parsed.get("Measured Power").unwrap(), "1000");
        assert_eq!(parsed.get("Requested Battery Power").unwrap(), "500");
    }
}
