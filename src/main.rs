//! PowerScraper - A multithreaded application to scrape inverter metrics, manage battery power rates, and forward metrics to monitoring systems.

#![allow(
    clippy::collapsible_if,
    clippy::redundant_closure,
    clippy::neg_multiply,
    clippy::io_other_error
)]

mod config;
mod mqtt_helper;
mod drivers {
    pub mod mqtt_meter;
    pub mod serial_meters;
    pub mod solax_modbus;
    pub mod solax_wifi;
}
mod forwarders;
mod power_manager;

use config::Config;
use std::path::Path;
use tokio::signal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read and parse configuration file
    let config_path = "config.toml";
    if !Path::new(config_path).exists() {
        println!("Error: config.toml not found! Please create one based on config-sample.toml");
        std::process::exit(1);
    }

    let config = match Config::load_from_file(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            println!("Error parsing config.toml: {}", e);
            std::process::exit(1);
        }
    };

    // 2. Validate MQTT broker config (mandatory for MQTT-decoupled communication)
    let mqtt_config = match config.mqtt {
        Some(ref cfg) => cfg.clone(),
        None => {
            println!(
                "Error: [MQTT] section is mandatory in config.toml for drivers and power management to communicate!"
            );
            std::process::exit(1);
        }
    };

    println!("Starting PowerScraper (Rust Next Branch)...");

    // 3. Spawn Solax Wifi Drivers
    if let Some(ref wifi_cfg) = config.solax_wifi {
        println!("Spawning Solax Wifi Drivers...");
        for host in &wifi_cfg.inverters {
            let host_clone = host.clone();
            let cfg_clone = wifi_cfg.clone();
            let mqtt_clone = mqtt_config.clone();
            tokio::spawn(async move {
                drivers::solax_wifi::run_solax_wifi_driver(host_clone, cfg_clone, mqtt_clone).await;
            });
        }
    }

    // 4. Spawn Solax Modbus TCP Drivers
    if let Some(ref modbus_cfg) = config.solax_modbus {
        println!("Spawning Solax Modbus TCP Drivers...");
        let hostnames = modbus_cfg.hostnames.clone().unwrap_or_default();
        for (idx, inverter) in modbus_cfg.inverters.iter().enumerate() {
            let hostname = if idx < hostnames.len() {
                hostnames[idx].clone()
            } else {
                format!("{}:502", inverter)
            };
            let inv_clone = inverter.clone();
            let cfg_clone = modbus_cfg.clone();
            let mqtt_clone = mqtt_config.clone();
            tokio::spawn(async move {
                drivers::solax_modbus::run_solax_modbus_driver(
                    inv_clone, hostname, cfg_clone, mqtt_clone,
                )
                .await;
            });
        }
    }

    // 5. Spawn Solax XHybrid Modbus TCP Drivers
    if let Some(ref hybrid_cfg) = config.solax_xhybrid_modbus {
        println!("Spawning Solax XHybrid Modbus TCP Drivers...");
        let hostnames = hybrid_cfg.hostnames.clone().unwrap_or_default();
        for (idx, inverter) in hybrid_cfg.inverters.iter().enumerate() {
            let hostname = if idx < hostnames.len() {
                hostnames[idx].clone()
            } else {
                format!("{}:502", inverter)
            };
            let inv_clone = inverter.clone();
            let cfg_clone = hybrid_cfg.clone();
            let mqtt_clone = mqtt_config.clone();
            tokio::spawn(async move {
                drivers::solax_modbus::run_solax_xhybrid_driver(
                    inv_clone, hostname, cfg_clone, mqtt_clone,
                )
                .await;
            });
        }
    }

    // 6. Spawn SDM630 Serial Modbus RTU Meters
    if let Some(ref sdm_cfg) = config.sdm630_modbus_v2 {
        println!("Spawning SDM630 Meter Drivers...");
        for port in &sdm_cfg.ports {
            let port_clone = port.clone();
            let cfg_clone = sdm_cfg.clone();
            let mqtt_clone = mqtt_config.clone();
            tokio::spawn(async move {
                drivers::serial_meters::run_sdm630_driver(port_clone, cfg_clone, mqtt_clone).await;
            });
        }
    }

    // 7. Spawn DTSU666 Serial Modbus RTU Meters
    if let Some(ref dtsu_cfg) = config.dtsu666 {
        println!("Spawning DTSU666 Meter Drivers...");
        for port in &dtsu_cfg.ports {
            let port_clone = port.clone();
            let cfg_clone = dtsu_cfg.clone();
            let mqtt_clone = mqtt_config.clone();
            tokio::spawn(async move {
                drivers::serial_meters::run_dtsu666_driver(port_clone, cfg_clone, mqtt_clone).await;
            });
        }
    }

    // 8. Spawn MQTT Custom Power Meters
    if let Some(ref mqtt_meter_cfg) = config.mqtt_power_meter {
        println!("Spawning MQTT Meter Bridge Drivers...");
        for name in &mqtt_meter_cfg.meters {
            if let Some(dev_cfg) = mqtt_meter_cfg.meter_devices.get(name) {
                let name_clone = name.clone();
                let dev_cfg_clone = dev_cfg.clone();
                let mqtt_clone = mqtt_config.clone();
                tokio::spawn(async move {
                    drivers::mqtt_meter::run_mqtt_meter_driver(
                        name_clone,
                        dev_cfg_clone,
                        mqtt_clone,
                    )
                    .await;
                });
            }
        }
    }

    // 9. Spawn Power Manager (Solax-BatteryControl)
    if let Some(ref battery_cfg) = config.battery_control {
        println!("Spawning Power Manager (Battery Control)...");
        let cfg_clone = battery_cfg.clone();
        let mqtt_clone = mqtt_config.clone();
        tokio::spawn(async move {
            power_manager::run_power_manager_task(cfg_clone, mqtt_clone).await;
        });
    }

    // 10. Spawn InfluxDB / EmonCMS Output Forwarders
    if config.emoncms.is_some() || config.influx.is_some() {
        println!("Spawning Output Forwarders (EmonCMS/InfluxDB)...");
        let emon_cfg = config.emoncms.clone();
        let influx_cfg = config.influx.clone();
        let mqtt_clone = mqtt_config.clone();
        tokio::spawn(async move {
            forwarders::run_forwarders_task(emon_cfg, influx_cfg, mqtt_clone).await;
        });
    }

    // Wait for termination signal
    println!("PowerScraper started. Press Ctrl-C to exit.");
    signal::ctrl_c().await?;
    println!("Shutdown signal received. Exiting...");

    Ok(())
}
