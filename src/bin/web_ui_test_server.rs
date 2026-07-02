use PowerScraper::config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = "test_web_ui.db".to_string();
    let _ = std::fs::remove_file(&db_path);

    // Load sample config and seed DB
    let config = Config::load_from_file("tests/test_config.toml")?;
    config.save_to_db(&db_path)?;

    // Seed some mock historical telemetry for the simulation and orientation inference tabs
    {
        use PowerScraper::power_manager::HistoryRecord;
        let mut buffer = Vec::new();
        let now_ts = chrono::Local::now().timestamp();
        // Generate 1 day of history (1440 points, once every 60 seconds)
        for i in 0..1440 {
            let ts = now_ts - (1440 - i) * 60;
            let minute_of_day = (i % 1440) as f64;
            
            // Solar power: bell curve peak at midday
            let solar_power = if minute_of_day > 360.0 && minute_of_day < 1080.0 {
                let x = (minute_of_day - 720.0) / 180.0;
                (3500.0 * (-x * x).exp()).max(0.0)
            } else {
                0.0
            };

            // Load power: double peak (morning and evening)
            let load_power = 800.0
                + 1200.0 * (-((minute_of_day - 480.0) / 60.0).powi(2)).exp()
                + 2500.0 * (-((minute_of_day - 1140.0) / 120.0).powi(2)).exp();

            // Battery power: discharging in evening/morning, charging midday
            let net_power = load_power - solar_power;
            let battery_power = if net_power > 0.0 {
                net_power.min(2000.0) // discharge
            } else {
                net_power.max(-2000.0) // charge
            };

            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "MainsMeter/Total system power".to_string(),
                value: load_power,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "SolaX-Hybrid-Meter/Total system power".to_string(),
                value: load_power,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "solax-modbus/PV1 Power".to_string(),
                value: solar_power,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "solax-modbus/Battery Power".to_string(),
                value: battery_power,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/import_price".to_string(),
                value: 28.5,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/export_price".to_string(),
                value: 8.2,
            });
        }
        PowerScraper::database::init_history_db(&db_path)?;
        PowerScraper::database::flush_history_to_db(&db_path, &mut buffer, None);
    }

    // Set some mock data in the global system status for telemetry UI testing
    {
        let mut status = PowerScraper::web_server::get_system_status().lock().unwrap();
        status.active_mode = "Auto".to_string();
        status.grid_target = 100.0;
        status.meter_power = -250.5;
        status.mqtt_connected = true;
        status.import_price = Some(28.5);
        status.export_price = Some(8.2);
        status.usage = Some(4250.0);
        status.power_budget = Some(6450.0);
        status.power_budget_with_charging = Some(1500.0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        status.meter_last_updated = Some(now - 5);
        
        let mut invs = std::collections::HashMap::new();
        invs.insert("solax-modbus".to_string(), PowerScraper::web_server::InverterStatus {
            battery_capacity: 85,
            battery_power: -1200,
            pv_power: 3200,
            run_mode: 2,
            last_updated: Some(now - 15),
            calculated_battery_capacity: Some(13.82),
            requested_power: Some(-1000),
        });
        invs.insert("solax-xhybrid".to_string(), PowerScraper::web_server::InverterStatus {
            battery_capacity: 90,
            battery_power: 1000,
            pv_power: 1500,
            run_mode: 2,
            last_updated: Some(now - 45),
            calculated_battery_capacity: Some(13.82),
            requested_power: Some(500),
        });
        status.inverters = invs;
    }

    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::channel::<()>(10);
    
    // Spawn task to drain reload signals and update system status
    let db_path_clone = db_path.clone();
    tokio::spawn(async move {
        while let Some(_) = reload_rx.recv().await {
            println!("Test server received config reload signal");
            // Load the updated config from DB and update status
            if let Ok(cfg) = Config::load_from_db(&db_path_clone) {
                if let Some(ref bc) = cfg.battery_control {
                    let mut status = PowerScraper::web_server::get_system_status().lock().unwrap();
                    if let Some(ref mode) = bc.initial_mode {
                        println!("Test server: Updating active_mode in status to: {}", mode);
                        status.active_mode = mode.clone();
                    }
                    if let Some(target) = bc.grid_target {
                        println!("Test server: Updating grid_target in status to: {}", target);
                        status.grid_target = target;
                    }
                }
            }
        }
    });

    println!("Starting Web UI Test Server on port 3000...");
    PowerScraper::web_server::run_web_server(reload_tx, db_path).await;
    Ok(())
}
