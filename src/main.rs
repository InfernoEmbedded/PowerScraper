//! PowerScraper - A multithreaded application to scrape inverter metrics, manage battery power rates, and forward metrics to monitoring systems.

#![allow(
    clippy::collapsible_if,
    clippy::redundant_closure,
    clippy::neg_multiply,
    clippy::io_other_error
)]

use PowerScraper::{config, drivers, forwarders, power_manager};
use config::Config;
use tokio::signal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting PowerScraper (Rust Next Branch)...");

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let db_path = "config.db".to_string();

    // Check for CSV import CLI subcommand
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && (args[1] == "import-csv" || args[1] == "import") {
        if args.len() < 3 {
            eprintln!("Usage: {} import-csv <csv_file_path>", args[0]);
            std::process::exit(1);
        }
        let csv_path = &args[2];
        if let Err(e) = PowerScraper::csv_importer::run_csv_import(&db_path, csv_path) {
            eprintln!("Error during CSV import: {}", e);
            std::process::exit(1);
        }
        println!("Import completed successfully.");
        std::process::exit(0);
    }


    // Create channel for reload trigger
    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Spawn Web server on dedicated thread
    let db_path_clone = db_path.clone();
    tokio::spawn(async move {
        PowerScraper::web_server::run_web_server(reload_tx, db_path_clone).await;
    });

    // Spawn watchdog task to detect MainsMeter freezes and restart process
    let db_path_wd = db_path.clone();
    tokio::spawn(async move {
        println!("Spawning watchdog task...");
        let start_time = std::time::Instant::now();
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            
            // Load config to check if a source is configured
            let has_source = if let Ok(cfg) = Config::load_from_db(&db_path_wd) {
                cfg.battery_control.as_ref().and_then(|bc| bc.source.as_ref()).is_some()
            } else {
                false
            };
            
            if has_source {
                let last_update = {
                    if let Ok(status) = PowerScraper::web_server::get_system_status().lock() {
                        status.meter_last_updated
                    } else {
                        None
                    }
                };
                
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                    
                match last_update {
                    Some(ts) => {
                        if now_secs > ts && now_secs - ts > 180 {
                            eprintln!("WATCHDOG: MainsMeter has not updated for {} seconds. Exiting for systemd restart...", now_secs - ts);
                            std::process::exit(1);
                        }
                    }
                    None => {
                        // Allow 5 minutes from startup for initial update
                        if start_time.elapsed() > tokio::time::Duration::from_secs(300) {
                            eprintln!("WATCHDOG: MainsMeter has failed to update since startup (5 minutes ago). Exiting for systemd restart...");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    });

    loop {
        // Load configuration from database
        let config = match Config::load_from_db(&db_path) {
            Ok(cfg) => cfg,
            Err(e) => {
                println!("Error loading configuration from database: {}", e);
                // Sleep before retry to prevent busy looping
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let mqtt_config = match config.mqtt {
            Some(ref cfg) => cfg.clone(),
            None => {
                println!(
                    "Warning: [MQTT] section not found in configuration. Defaulting to local broker at homeautemation.lan."
                );
                config::MqttBrokerConfig {
                    broker: "homeautemation.lan".to_string(),
                    port: Some(1883),
                    base_topic: Some("sensors".to_string()),
                    username: Some("power".to_string()),
                    password: Some("d=5Pqjkh{9".to_string()),
                    home_assistant_discovery: Some(true),
                    home_assistant_prefix: Some("homeassistant".to_string()),
                }
            }
        };

        // Create cancellation token for this round of tasks
        let cancel_token = tokio_util::sync::CancellationToken::new();

        println!("Spawning background drivers and manager tasks...");

        // 3. Spawn Solax Wifi Drivers
        if let Some(ref wifi_cfg) = config.solax_wifi {
            println!("Spawning Solax Wifi Drivers...");
            for host in &wifi_cfg.inverters {
                let host_clone = host.clone();
                let cfg_clone = wifi_cfg.clone();
                let mqtt_clone = mqtt_config.clone();
                let cancel_clone = cancel_token.clone();
                tokio::spawn(async move {
                    drivers::solax_wifi::run_solax_wifi_driver(
                        host_clone,
                        cfg_clone,
                        mqtt_clone,
                        cancel_clone,
                    )
                    .await;
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
                let cancel_clone = cancel_token.clone();
                tokio::spawn(async move {
                    drivers::solax_modbus::run_solax_modbus_driver(
                        inv_clone,
                        hostname,
                        cfg_clone,
                        mqtt_clone,
                        cancel_clone,
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
                let cancel_clone = cancel_token.clone();
                tokio::spawn(async move {
                    drivers::solax_xhybrid::run_solax_xhybrid_driver(
                        inv_clone,
                        hostname,
                        cfg_clone,
                        mqtt_clone,
                        cancel_clone,
                    )
                    .await;
                });
            }
        }

        // 6. Spawn SDM630 Serial Modbus RTU Meters
        if let Some(ref sdm_cfg) = config.sdm630_modbus_v2 {
            println!("Spawning SDM630 Meter Drivers on dedicated realtime threads...");
            for port in &sdm_cfg.ports {
                let port_clone = port.clone();
                let cfg_clone = sdm_cfg.clone();
                let mqtt_clone = mqtt_config.clone();
                let cancel_clone = cancel_token.clone();
                let device_name = port_clone.replace("/dev/tty", "");
                
                std::thread::Builder::new()
                    .name(format!("sdm630-{}", device_name))
                    .spawn(move || {
                        #[cfg(target_os = "linux")]
                        unsafe {
                            let thread_id = libc::pthread_self();
                            // SCHED_FIFO = 1
                            let policy = 1;
                            let param = libc::sched_param { sched_priority: 50 };
                            let res = libc::pthread_setschedparam(thread_id, policy, &param);
                            if res != 0 {
                                eprintln!("[Warning] Failed to set SDM630 thread to SCHED_FIFO: error code {}", res);
                            } else {
                                println!("Successfully set SDM630 thread to SCHED_FIFO (realtime priority 50)");
                            }
                        }
                        
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                            
                        rt.block_on(async {
                            drivers::sdm630::run_sdm630_driver(
                                port_clone,
                                cfg_clone,
                                mqtt_clone,
                                cancel_clone,
                            )
                            .await;
                        });
                    })
                    .expect("Failed to spawn SDM630 driver thread");
            }
        }

        // 7. Spawn DTSU666 Serial Modbus RTU Meters
        if let Some(ref dtsu_cfg) = config.dtsu666 {
            println!("Spawning DTSU666 Meter Drivers on dedicated realtime threads...");
            for port in &dtsu_cfg.ports {
                let port_clone = port.clone();
                let cfg_clone = dtsu_cfg.clone();
                let mqtt_clone = mqtt_config.clone();
                let cancel_clone = cancel_token.clone();
                let device_name = port_clone.replace("/dev/tty", "");
                
                std::thread::Builder::new()
                    .name(format!("dtsu666-{}", device_name))
                    .spawn(move || {
                        #[cfg(target_os = "linux")]
                        unsafe {
                            let thread_id = libc::pthread_self();
                            // SCHED_FIFO = 1
                            let policy = 1;
                            let param = libc::sched_param { sched_priority: 50 };
                            let res = libc::pthread_setschedparam(thread_id, policy, &param);
                            if res != 0 {
                                eprintln!("[Warning] Failed to set DTSU666 thread to SCHED_FIFO: error code {}", res);
                            } else {
                                println!("Successfully set DTSU666 thread to SCHED_FIFO (realtime priority 50)");
                            }
                        }
                        
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                            
                        rt.block_on(async {
                            drivers::dtsu666::run_dtsu666_driver(
                                port_clone,
                                cfg_clone,
                                mqtt_clone,
                                cancel_clone,
                            )
                            .await;
                        });
                    })
                    .expect("Failed to spawn DTSU666 driver thread");
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
                    let cancel_clone = cancel_token.clone();
                    tokio::spawn(async move {
                        drivers::mqtt_meter::run_mqtt_meter_driver(
                            name_clone,
                            dev_cfg_clone,
                            mqtt_clone,
                            cancel_clone,
                        )
                        .await;
                    });
                }
            }
        }

        // Spawn MQTT Inverters
        if let Some(ref mqtt_inv_cfg) = config.mqtt_inverter {
            println!("Spawning MQTT Inverter Drivers...");
            for name in &mqtt_inv_cfg.inverters {
                if let Some(dev_cfg) = mqtt_inv_cfg.inverter_devices.get(name) {
                    let name_clone = name.clone();
                    let dev_cfg_clone = dev_cfg.clone();
                    let mqtt_clone = mqtt_config.clone();
                    let cancel_clone = cancel_token.clone();
                    tokio::spawn(async move {
                        drivers::mqtt_inverter::run_mqtt_inverter_driver(
                            name_clone,
                            dev_cfg_clone,
                            mqtt_clone,
                            cancel_clone,
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
            let cancel_clone = cancel_token.clone();
            let db_path_pm = db_path.clone();
            tokio::spawn(async move {
                power_manager::run_power_manager_task(cfg_clone, mqtt_clone, cancel_clone, db_path_pm).await;
            });
        }

        // 10. Spawn InfluxDB / EmonCMS Output Forwarders
        if config.emoncms.is_some() || config.influx.is_some() {
            println!("Spawning Output Forwarders (EmonCMS/InfluxDB)...");
            let emon_cfg = config.emoncms.clone();
            let influx_cfg = config.influx.clone();
            let mqtt_clone = mqtt_config.clone();
            let cancel_clone = cancel_token.clone();
            tokio::spawn(async move {
                forwarders::run_forwarders_task(emon_cfg, influx_cfg, mqtt_clone, cancel_clone)
                    .await;
            });
        }

        // Wait until reload signal or Ctrl-C shutdown
        tokio::select! {
            res = reload_rx.recv() => {
                if let Some(_) = res {
                    println!("Reload signal received. Canceling old drivers and reloading config...");
                    cancel_token.cancel();
                    // Sleep briefly to let drivers clean up connections
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                } else {
                    println!("Reload channel closed! Web server has stopped. Waiting for shutdown signal to exit...");
                    tokio::select! {
                        _ = signal::ctrl_c() => {}
                        _ = async {
                            #[cfg(unix)]
                            {
                                if let Some(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok() {
                                    sig.recv().await;
                                }
                            }
                            #[cfg(not(unix))]
                            {
                                tokio::time::sleep(std::time::Duration::from_secs(999999)).await;
                            }
                        } => {}
                    }
                    println!("Shutdown signal received. Canceling tasks...");
                    cancel_token.cancel();
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    break;
                }
            }
            _ = signal::ctrl_c() => {
                println!("Shutdown signal (Ctrl-C) received. Canceling tasks...");
                cancel_token.cancel();
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                break;
            }
            _ = async {
                #[cfg(unix)]
                {
                    sigterm.recv().await;
                }
                #[cfg(not(unix))]
                {
                    tokio::time::sleep(std::time::Duration::from_secs(999999)).await;
                }
            } => {
                println!("Shutdown signal (SIGTERM) received. Canceling tasks...");
                cancel_token.cancel();
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                break;
            }
        }
    }

    Ok(())
}
