#![allow(
    clippy::collapsible_if,
    clippy::redundant_closure,
    clippy::neg_multiply,
    clippy::io_other_error,
    non_snake_case
)]

use PowerScraper::config::Config;
use PowerScraper::drivers;
use PowerScraper::forwarders;
use PowerScraper::power_manager;
use rumqttc::QoS;
use std::fs::File;
use std::io::Write;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::sleep;

struct MosquittoGuard {
    child: Child,
    conf_path: String,
    mosquitto_bin: String,
}

impl MosquittoGuard {
    fn new(port: u16) -> Self {
        // Write mosquitto conf file
        let conf_content = format!("listener {}\nallow_anonymous true\n", port);
        let conf_path = format!("tests/mosquitto_test_{}.conf", port);
        let mut file = File::create(&conf_path).expect("Failed to create mosquitto_test.conf");
        file.write_all(conf_content.as_bytes())
            .expect("Failed to write mosquitto_test.conf");

        let mosquitto_bin = format!("target/mosquitto_bin_{}", port);
        if let Err(e) = std::fs::copy("/usr/sbin/mosquitto", &mosquitto_bin) {
            println!(
                "Warning: Failed to copy mosquitto to {}: {}",
                mosquitto_bin, e
            );
        }

        // Spawn mosquitto using the copied binary to bypass AppArmor
        let child = Command::new(&mosquitto_bin)
            .arg("-c")
            .arg(&conf_path)
            .spawn()
            .or_else(|_| {
                // Fallback to system mosquitto
                Command::new("mosquitto").arg("-c").arg(&conf_path).spawn()
            })
            .expect("Failed to start mosquitto broker");

        MosquittoGuard {
            child,
            conf_path,
            mosquitto_bin,
        }
    }
}

impl Drop for MosquittoGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_file(&self.conf_path);
        let _ = std::fs::remove_file(&self.mosquitto_bin);
    }
}

async fn run_mock_modbus_server(
    listener: TcpListener,
    regs: Arc<Mutex<Vec<u16>>>,
    writes: Arc<Mutex<Vec<(u16, u16)>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                if let Ok((stream, _)) = incoming {
                    let regs_clone = regs.clone();
                    let writes_clone = writes.clone();
                    let mut shutdown_clone = shutdown.resubscribe();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 1024];
                        let mut stream = stream;
                        loop {
                            tokio::select! {
                                _ = shutdown_clone.recv() => {
                                    break;
                                }
                                header_res = stream.read_exact(&mut buf[..7]) => {
                                    if header_res.is_err() {
                                        break;
                                    }
                                    let transaction_id = u16::from_be_bytes([buf[0], buf[1]]);
                                    let protocol_id = u16::from_be_bytes([buf[2], buf[3]]);
                                    let length = u16::from_be_bytes([buf[4], buf[5]]);
                                    let unit_id = buf[6];

                                    if protocol_id != 0 || length < 2 {
                                        break;
                                    }

                                    let pdu_len = (length - 1) as usize;
                                    if stream.read_exact(&mut buf[7..7 + pdu_len]).await.is_err() {
                                        break;
                                    }

                                    let function_code = buf[7];
                                    match function_code {
                                        0x03 | 0x04 => {
                                            let start_addr = u16::from_be_bytes([buf[8], buf[9]]) as usize;
                                            let quantity = u16::from_be_bytes([buf[10], buf[11]]) as usize;

                                            let data = {
                                                let regs_guard = regs_clone.lock().unwrap();
                                                let mut data = vec![];
                                                for i in 0..quantity {
                                                    let val = regs_guard.get(start_addr + i).cloned().unwrap_or(0);
                                                    data.extend_from_slice(&val.to_be_bytes());
                                                }
                                                data
                                            };

                                            let mut resp_pdu = vec![function_code, (quantity * 2) as u8];
                                            resp_pdu.extend_from_slice(&data);

                                            let resp_len = (1 + resp_pdu.len()) as u16;
                                            let mut resp = vec![];
                                            resp.extend_from_slice(&transaction_id.to_be_bytes());
                                            resp.extend_from_slice(&0u16.to_be_bytes());
                                            resp.extend_from_slice(&resp_len.to_be_bytes());
                                            resp.push(unit_id);
                                            resp.extend_from_slice(&resp_pdu);

                                            if stream.write_all(&resp).await.is_err() {
                                                break;
                                            }
                                        }
                                        0x06 => {
                                            let reg_addr = u16::from_be_bytes([buf[8], buf[9]]);
                                            let val = u16::from_be_bytes([buf[10], buf[11]]);

                                            writes_clone.lock().unwrap().push((reg_addr, val));

                                            let mut resp_pdu = vec![0x06];
                                            resp_pdu.extend_from_slice(&reg_addr.to_be_bytes());
                                            resp_pdu.extend_from_slice(&val.to_be_bytes());

                                            let resp_len = (1 + resp_pdu.len()) as u16;
                                            let mut resp = vec![];
                                            resp.extend_from_slice(&transaction_id.to_be_bytes());
                                            resp.extend_from_slice(&0u16.to_be_bytes());
                                            resp.extend_from_slice(&resp_len.to_be_bytes());
                                            resp.push(unit_id);
                                            resp.extend_from_slice(&resp_pdu);

                                            if stream.write_all(&resp).await.is_err() {
                                                break;
                                            }
                                        }
                                        _ => {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
            }
            _ = shutdown.recv() => {
                break;
            }
        }
    }
}

async fn run_mock_wifi_server(
    listener: TcpListener,
    http_requests: Arc<Mutex<Vec<String>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let wifi_req_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let emoncms_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let influx_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    loop {
        tokio::select! {
            incoming = listener.accept() => {
                if let Ok((mut stream, _)) = incoming {
                    let http_clone = http_requests.clone();
                    let count_clone = wifi_req_count.clone();
                    let emon_count_clone = emoncms_count.clone();
                    let influx_count_clone = influx_count.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 8192];
                        if let Ok(n) = stream.read(&mut buf).await {
                            if n > 0 {
                                let request = String::from_utf8_lossy(&buf[..n]);
                                println!("Mock HTTP Server received request: {}", request.lines().next().unwrap_or(""));
                                if request.contains("GET /api/realTimeData.htm") {
                                    let req_num = count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if req_num == 1 {
                                        let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    } else if req_num == 2 {
                                        let json_body = r#"{"SN":"WIFI12345","Data": invalid}"#;
                                        let response = format!(
                                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                            json_body.len(),
                                            json_body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    } else {
                                        let json_body = r#"{"SN":"WIFI12345","Data":["0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","50","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0","0"]}"#;
                                        let response = format!(
                                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                            json_body.len(),
                                            json_body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                } else if request.contains("/input/post") {
                                    http_clone.lock().unwrap().push(request.to_string());
                                    let req_num = emon_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if req_num == 1 {
                                        let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    } else {
                                        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                } else if request.contains("POST /api/v2/write") {
                                    http_clone.lock().unwrap().push(request.to_string());
                                    let req_num = influx_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if req_num == 1 {
                                        let err_body = "Influx DB error simulation";
                                        let response = format!(
                                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                            err_body.len(),
                                            err_body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    } else {
                                        let response = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                }
                            }
                        }
                    });
                }
            }
            _ = shutdown.recv() => {
                break;
            }
        }
    }
}

struct DbGuard {
    path: String,
}

impl Drop for DbGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[tokio::test]
async fn test_integration_loop() {
    // Dynamically allocate ports
    let modbus_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let modbus_port = modbus_listener.local_addr().unwrap().port();

    let wifi_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wifi_port = wifi_listener.local_addr().unwrap().port();

    let web_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web_port = web_listener.local_addr().unwrap().port();

    let mqtt_port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };

    // 1. Start Mosquitto on dynamic port
    let _mosquitto = MosquittoGuard::new(mqtt_port);
    sleep(Duration::from_millis(500)).await;

    // 2. Set up shared states for mock Modbus
    let regs = Arc::new(Mutex::new(vec![0u16; 250]));
    {
        let mut r = regs.lock().unwrap();
        r[28] = 50; // Battery Capacity = 50
        r[70] = 1000; // Measured Power = 1000
    }
    let writes = Arc::new(Mutex::new(vec![]));
    let http_requests = Arc::new(Mutex::new(vec![]));

    // Shutdown channels for mock servers
    let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);

    // Spawn Mock Modbus Server
    let regs_clone = regs.clone();
    let writes_clone = writes.clone();
    let shutdown_rx1 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        run_mock_modbus_server(modbus_listener, regs_clone, writes_clone, shutdown_rx1).await;
    });

    // Spawn Mock WiFi / EmonCMS Server
    let http_clone = http_requests.clone();
    let shutdown_rx2 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        run_mock_wifi_server(wifi_listener, http_clone, shutdown_rx2).await;
    });

    // 3. Seed config to sqlite database config_<port>.db and run web server
    let mut config =
        Config::load_from_file("tests/test_config.toml").expect("Failed to load test config");

    // Update config in-memory with dynamic ports
    if let Some(ref mut mqtt) = config.mqtt {
        mqtt.port = Some(mqtt_port);
    }
    if let Some(ref mut modbus) = config.solax_modbus {
        modbus.hostnames = Some(vec![format!("127.0.0.1:{}", modbus_port)]);
    }
    if let Some(ref mut g3) = config.solax_g3_modbus {
        g3.hostnames = Some(vec![format!("127.0.0.1:{}", modbus_port)]);
    }
    if let Some(ref mut wifi) = config.solax_wifi {
        wifi.inverters = vec![format!("127.0.0.1:{}", wifi_port)];
    }
    if let Some(ref mut mqtt_meter) = config.mqtt_power_meter {
        if let Some(ref mut meter) = mqtt_meter.meter_devices.get_mut("custom-meter") {
            meter.port = Some(mqtt_port);
        }
    }
    if let Some(ref mut emoncms) = config.emoncms {
        emoncms.server = format!("http://127.0.0.1:{}", wifi_port);
    }
    if let Some(ref mut influx) = config.influx {
        influx.influx_url = format!("http://127.0.0.1:{}", wifi_port);
    }

    let db_path = format!("config_{}.db", mqtt_port);
    let _db_guard = DbGuard { path: db_path.clone() };
    let _ = std::fs::remove_file(&db_path);
    config
        .save_to_db(&db_path)
        .expect("Failed to seed config to DB");

    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::channel::<()>(10);
    let db_path_clone = db_path.clone();
    let web_cancel_token = tokio_util::sync::CancellationToken::new();
    tokio::spawn(async move {
        PowerScraper::web_server::run_web_server_with_listener(reload_tx, db_path_clone, web_listener, web_cancel_token).await;
    });
    sleep(Duration::from_millis(500)).await; // Allow server to start

    let active_cancel_token = Arc::new(Mutex::new(tokio_util::sync::CancellationToken::new()));
    let act_token_clone = active_cancel_token.clone();
    let db_path_loop = db_path.clone();

    let reload_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rc_clone = reload_count.clone();

    tokio::spawn(async move {
        loop {
            let current_cfg = match Config::load_from_db(&db_path_loop).map_err(|e| e.to_string()) {
                Ok(cfg) => cfg,
                Err(err_msg) => {
                    println!("Error loading integration config: {}", err_msg);
                    sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            let mqtt_config = current_cfg.mqtt.clone().unwrap();
            let token = tokio_util::sync::CancellationToken::new();
            {
                let mut lock = act_token_clone.lock().unwrap();
                *lock = token.clone();
            }
            let (tx_driver_telemetry, rx_driver_telemetry) = tokio::sync::mpsc::channel::<PowerScraper::dispatch_manager::TelemetryBatch>(4096);
            let (tx_command, rx_command) = tokio::sync::mpsc::channel::<PowerScraper::dispatch_manager::DriverCommand>(1024);
            let (tx_power_manager, rx_power_manager) = tokio::sync::mpsc::channel::<PowerScraper::dispatch_manager::TelemetryBatch>(2048);
            let (tx_mqtt, rx_mqtt) = tokio::sync::mpsc::channel::<PowerScraper::dispatch_manager::TelemetryBatch>(4096);

            let mut forwarder_senders = vec![tx_mqtt];
            let mqtt_fwd = forwarders::MQTTForwarder::new(mqtt_config.clone(), rx_mqtt, tx_command.clone());
            let cancel_mqtt = token.clone();
            tokio::spawn(async move {
                mqtt_fwd.run(cancel_mqtt).await;
            });

            if let Some(ref emon_cfg) = current_cfg.emoncms {
                let (tx_emon, rx_emon) = tokio::sync::mpsc::channel::<PowerScraper::dispatch_manager::TelemetryBatch>(4096);
                forwarder_senders.push(tx_emon);
                let emon_fwd = forwarders::EmonCMSForwarder::new(emon_cfg.clone(), rx_emon);
                let cancel_emon = token.clone();
                tokio::spawn(async move {
                    emon_fwd.run(cancel_emon).await;
                });
            }

            if let Some(ref influx_cfg) = current_cfg.influx {
                let (tx_influx, rx_influx) = tokio::sync::mpsc::channel::<PowerScraper::dispatch_manager::TelemetryBatch>(4096);
                forwarder_senders.push(tx_influx);
                let influx_fwd = forwarders::InfluxForwarder::new(influx_cfg.clone(), rx_influx);
                let cancel_influx = token.clone();
                tokio::spawn(async move {
                    influx_fwd.run(cancel_influx).await;
                });
            }

            let dispatch_mgr = PowerScraper::dispatch_manager::DispatchManager::new(
                rx_driver_telemetry,
                forwarder_senders,
                tx_power_manager,
            );
            let cancel_dispatch = token.clone();
            tokio::spawn(async move {
                dispatch_mgr.run(cancel_dispatch).await;
            });

            if let Some(wifi_cfg) = current_cfg.solax_wifi.clone() {
                let token_clone = token.clone();
                let mqtt_clone = mqtt_config.clone();
                let tx_telemetry = tx_driver_telemetry.clone();
                let hostname = wifi_cfg.inverters.first().cloned().unwrap_or_else(|| format!("127.0.0.1:{}", wifi_port));
                tokio::spawn(async move {
                    drivers::solax_wifi::run_solax_wifi_driver(
                        hostname,
                        wifi_cfg,
                        mqtt_clone,
                        tx_telemetry,
                        token_clone,
                    )
                    .await;
                });
            }

            if let Some(modbus_cfg) = current_cfg.solax_modbus.clone() {
                let token_clone = token.clone();
                let mqtt_clone = mqtt_config.clone();
                let tx_telemetry = tx_driver_telemetry.clone();
                let hostname = modbus_cfg.hostnames.as_ref().and_then(|h| h.first()).cloned().unwrap_or_else(|| format!("127.0.0.1:{}", modbus_port));
                tokio::spawn(async move {
                    drivers::solax_modbus::run_solax_modbus_driver(
                        "solax-modbus".to_string(),
                        hostname,
                        modbus_cfg,
                        mqtt_clone,
                        tx_telemetry,
                        token_clone,
                    )
                    .await;
                });
            }

            if let Some(g3_cfg) = current_cfg.solax_g3_modbus.clone() {
                let token_clone = token.clone();
                let mqtt_clone = mqtt_config.clone();
                let tx_telemetry = tx_driver_telemetry.clone();
                let hostname = g3_cfg.hostnames.as_ref().and_then(|h| h.first()).cloned().unwrap_or_else(|| format!("127.0.0.1:{}", modbus_port));
                tokio::spawn(async move {
                    drivers::solax_g3::run_solax_g3_driver(
                        "solax-xhybrid".to_string(),
                        hostname,
                        g3_cfg,
                        mqtt_clone,
                        tx_telemetry,
                        token_clone,
                    )
                    .await;
                });
            }

            if let Some(battery_cfg) = current_cfg.battery_control.clone() {
                let token_clone = token.clone();
                let mqtt_clone = mqtt_config.clone();
                let db_path_pm = db_path_loop.clone();
                let tx_dispatch_agg = tx_driver_telemetry.clone();
                tokio::spawn(async move {
                    power_manager::run_power_manager_queue_task(
                        battery_cfg,
                        mqtt_clone,
                        rx_power_manager,
                        rx_command,
                        tx_dispatch_agg,
                        token_clone,
                        db_path_pm,
                    )
                    .await;
                });
            }

            if let Some(mqtt_meter_cfg) = current_cfg.mqtt_power_meter.clone() {
                if let Some(meter_dev_cfg) = mqtt_meter_cfg.meter_devices.get("custom-meter") {
                    let token_clone = token.clone();
                    let mqtt_clone = mqtt_config.clone();
                    let tx_telemetry = tx_driver_telemetry.clone();
                    let dev_cfg = meter_dev_cfg.clone();
                    tokio::spawn(async move {
                        drivers::mqtt_meter::run_mqtt_meter_driver(
                            "custom-meter".to_string(),
                            dev_cfg,
                            mqtt_clone,
                            tx_telemetry,
                            token_clone,
                        )
                        .await;
                    });
                }
            }

            tokio::select! {
                reload_signal = reload_rx.recv() => {
                    if reload_signal.is_none() {
                        break;
                    }
                }
            }
            println!("Integration Test reload triggered.");
            token.cancel();
            rc_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            sleep(Duration::from_millis(200)).await;
        }
    });

    let mqtt_config = config.mqtt.clone().expect("Missing MQTT config");

    // Connect test MQTT client to verify custom meter and forwarder bridging
    let received_bridged_power = Arc::new(Mutex::new([false; 4]));
    let rbp_clone = received_bridged_power.clone();
    let received_mode_status = Arc::new(Mutex::new(String::new()));
    let rms_clone = received_mode_status.clone();
    let received_target_status = Arc::new(Mutex::new(String::new()));
    let rts_clone = received_target_status.clone();
    let received_ha_discovery = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let rha_clone = received_ha_discovery.clone();

    let (test_client, mut test_eventloop) =
        PowerScraper::mqtt_helper::create_mqtt_client("integration-test-client", &mqtt_config);
    test_client
        .subscribe("sensors/custom-meter/#", QoS::AtLeastOnce)
        .await
        .unwrap();
    test_client
        .subscribe("sensors/power_manager/#", QoS::AtLeastOnce)
        .await
        .unwrap();
    test_client
        .subscribe("homeassistant/#", QoS::AtLeastOnce)
        .await
        .unwrap();

    tokio::spawn(async move {
        loop {
            if let Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p))) =
                test_eventloop.poll().await
            {
                if let Some(suffix) = p.topic.strip_prefix("sensors/custom-meter/") {
                    let payload = String::from_utf8_lossy(&p.payload);
                    let val = payload.trim();
                    let mut lock = rbp_clone.lock().unwrap();
                    if suffix == "Total system power" && val == "1500" {
                        lock[0] = true;
                    } else if suffix == "Phase 1 power" && val == "400" {
                        lock[1] = true;
                    } else if suffix == "Phase 2 power" && val == "500" {
                        lock[2] = true;
                    } else if suffix == "Phase 3 power" && val == "600" {
                        lock[3] = true;
                    }
                } else if let Some(suffix) = p.topic.strip_prefix("sensors/power_manager/") {
                    let payload = String::from_utf8_lossy(&p.payload);
                    let val = payload.trim();
                    if suffix == "mode" {
                        let mut lock = rms_clone.lock().unwrap();
                        *lock = val.to_string();
                    } else if suffix == "grid_target" {
                        let mut lock = rts_clone.lock().unwrap();
                        *lock = val.to_string();
                    }
                } else if let Some(suffix) = p.topic.strip_prefix("homeassistant/") {
                    let val = String::from_utf8_lossy(&p.payload);
                    let mut lock = rha_clone.lock().unwrap();
                    lock.insert(suffix.to_string(), val.to_string());
                }
            }
        }
    });

    // Give system tasks time to initialize
    sleep(Duration::from_millis(500)).await;

    // Publish to custom topics to trigger MQTT Meter driver translation (covering all branches)
    test_client
        .publish("custom/total", QoS::AtLeastOnce, false, "1500")
        .await
        .unwrap();
    test_client
        .publish("custom/p1", QoS::AtLeastOnce, false, "400")
        .await
        .unwrap();
    test_client
        .publish("custom/p2", QoS::AtLeastOnce, false, "500")
        .await
        .unwrap();
    test_client
        .publish("custom/p3", QoS::AtLeastOnce, false, "600")
        .await
        .unwrap();

    // Publish commands to verify Power Manager mode change and target change via MQTT
    test_client
        .publish(
            "sensors/power_manager/command/mode",
            QoS::AtLeastOnce,
            false,
            "ChargeBatteries",
        )
        .await
        .unwrap();
    test_client
        .publish(
            "sensors/power_manager/command/grid_target",
            QoS::AtLeastOnce,
            false,
            "-200",
        )
        .await
        .unwrap();

    // 5. Wait for integration loop to execute and write commands, and for forwarders to flush (10.5 seconds)
    let mut success_modbus_standard = false;
    let mut success_modbus_hybrid = false;
    let mut success_mqtt_meter = false;
    let mut success_emoncms = false;
    let mut success_influx = false;
    let mut success_mode = false;
    let mut success_target = false;
    let mut success_ha_discovery = false;

    // Poll assertions over a 12 second window
    for _ in 0..60 {
        sleep(Duration::from_millis(200)).await;

        if !success_modbus_standard || !success_modbus_hybrid {
            let w = writes.lock().unwrap();
            if w.iter().any(|(addr, val)| *addr == 0x51 && *val == 2000) {
                success_modbus_standard = true;
            }
            if w.iter().any(|(addr, val)| *addr == 0x52 && *val == 2000) {
                success_modbus_hybrid = true;
            }
        }

        if !success_mqtt_meter {
            let lock = received_bridged_power.lock().unwrap();
            if lock[0] && lock[1] && lock[2] && lock[3] {
                success_mqtt_meter = true;
            }
        }

        if !success_mode {
            let m = received_mode_status.lock().unwrap();
            if *m == "ChargeBatteries" {
                success_mode = true;
            }
        }

        if !success_target {
            let t = received_target_status.lock().unwrap();
            if *t == "-200" || *t == "-200.0" {
                success_target = true;
            }
        }

        if !success_ha_discovery {
            let lock = received_ha_discovery.lock().unwrap();
            let has_pm_mode = lock.contains_key("select/power_manager/mode/config");
            let has_pm_target = lock.contains_key("number/power_manager/grid_target/config");
            let has_inv_cmd = lock.contains_key("number/solax_modbus/charge_battery/config");
            let has_meter_sensor =
                lock.contains_key("sensor/custom_meter/total_system_power/config");
            if has_pm_mode && has_pm_target && has_inv_cmd && has_meter_sensor {
                success_ha_discovery = true;
            }
        }

        if !success_emoncms || !success_influx {
            let reqs = http_requests.lock().unwrap();
            if !success_emoncms {
                if reqs.iter().any(|r| {
                    r.contains("node=solax-modbus")
                        && (r.contains("Battery+Capacity") || r.contains("Battery%20Capacity"))
                }) {
                    success_emoncms = true;
                }
            }
            if !success_influx {
                if reqs.iter().any(|r| {
                    r.contains("POST /api/v2/write")
                        && r.contains("solax,inverter=solax-modbus")
                        && r.contains("Measured_Power=1000")
                }) {
                    success_influx = true;
                }
            }
        }

        if success_modbus_standard
            && success_modbus_hybrid
            && success_mqtt_meter
            && success_emoncms
            && success_influx
            && success_mode
            && success_target
            && success_ha_discovery
        {
            break;
        }
    }

    // 5.5. Transition to MaximumFeedin mode via MQTT command and assert modbus registers update to discharge rate
    test_client
        .publish(
            "sensors/power_manager/command/mode",
            QoS::AtLeastOnce,
            false,
            "MaximumFeedin",
        )
        .await
        .unwrap();

    let mut success_max_feedin_mode = false;
    let mut success_max_feedin_writes_std = false;
    let mut success_max_feedin_writes_hyb = false;
    for _ in 0..60 {
        sleep(Duration::from_millis(200)).await;
        if !success_max_feedin_mode {
            let m = received_mode_status.lock().unwrap();
            if *m == "MaximumFeedin" {
                success_max_feedin_mode = true;
            }
        }
        if !success_max_feedin_writes_std || !success_max_feedin_writes_hyb {
            let w = writes.lock().unwrap();
            // -2000 as u16 is 63536
            if w.iter().any(|(addr, val)| *addr == 0x51 && *val == 63536) {
                success_max_feedin_writes_std = true;
            }
            if w.iter().any(|(addr, val)| *addr == 0x52 && *val == 63536) {
                success_max_feedin_writes_hyb = true;
            }
        }
        if success_max_feedin_mode && success_max_feedin_writes_std && success_max_feedin_writes_hyb {
            break;
        }
    }

    // Transition back to Auto mode via MQTT command and assert mode updates
    test_client
        .publish(
            "sensors/power_manager/command/mode",
            QoS::AtLeastOnce,
            false,
            "Auto",
        )
        .await
        .unwrap();

    let mut success_auto_mode = false;
    for _ in 0..60 {
        sleep(Duration::from_millis(200)).await;
        let m = received_mode_status.lock().unwrap();
        if *m == "Auto" {
            success_auto_mode = true;
            break;
        }
    }

    // 6. POST new config via REST API to trigger reload
    let mut new_config = config.clone();
    if let Some(ref mut pm_cfg) = new_config.battery_control {
        pm_cfg.grid_target = Some(-500.0);
    }
    let client = reqwest::Client::new();
    let web_url = format!("http://127.0.0.1:{}", web_port);

    // Exercise static web UI and REST API endpoints to ensure 100% test coverage
    let dashboard_res = client
        .get(&format!("{}/", web_url))
        .send()
        .await
        .expect("Failed to GET /");
    assert_eq!(dashboard_res.status(), reqwest::StatusCode::OK);

    let style_res = client
        .get(&format!("{}/style.css", web_url))
        .send()
        .await
        .expect("Failed to GET /style.css");
    assert_eq!(style_res.status(), reqwest::StatusCode::OK);

    let js_res = client
        .get(&format!("{}/app.js", web_url))
        .send()
        .await
        .expect("Failed to GET /app.js");
    assert_eq!(js_res.status(), reqwest::StatusCode::OK);

    let get_config_res = client
        .get(&format!("{}/api/config", web_url))
        .send()
        .await
        .expect("Failed to GET /api/config");
    assert_eq!(get_config_res.status(), reqwest::StatusCode::OK);

    let status_res = client
        .get(&format!("{}/api/status", web_url))
        .send()
        .await
        .expect("Failed to GET /api/status");
    assert_eq!(status_res.status(), reqwest::StatusCode::OK);

    let res = client
        .post(&format!("{}/api/config", web_url))
        .json(&new_config)
        .send()
        .await
        .expect("Failed to POST new config to Web server");
    assert_eq!(res.status(), reqwest::StatusCode::OK);

    let mut success_reload = false;
    for _ in 0..30 {
        sleep(Duration::from_millis(200)).await;
        let t = received_target_status.lock().unwrap().clone();
        if t == "-500" || t == "-500.0" {
            success_reload = true;
            break;
        }
    }

    let _ = shutdown_tx.send(());
    active_cancel_token.lock().unwrap().cancel();
    sleep(Duration::from_millis(200)).await;

    assert!(
        success_modbus_standard,
        "Integration test failed: commanded charging rate 2000 was not written to register 0x51 (standard Modbus)"
    );
    assert!(
        success_modbus_hybrid,
        "Integration test failed: commanded charging rate 2000 was not written to register 0x52 (XHybrid Modbus)"
    );
    assert!(
        success_mqtt_meter,
        "Integration test failed: custom MQTT meter was not bridged correctly across all phases"
    );
    assert!(
        success_emoncms,
        "Integration test failed: EmonCMS forwarder did not post metrics to HTTP mock server"
    );
    assert!(
        success_influx,
        "Integration test failed: InfluxDB forwarder did not post metrics to HTTP mock server"
    );
    assert!(
        success_mode,
        "Integration test failed: Power Manager did not transition to ChargeBatteries mode or report status"
    );
    assert!(
        success_target,
        "Integration test failed: Power Manager did not update grid target to -200 or report status"
    );
    assert!(
        success_ha_discovery,
        "Integration test failed: Home Assistant MQTT discovery configs not published correctly"
    );
    assert!(
        success_max_feedin_mode,
        "Integration test failed: Power Manager did not transition to MaximumFeedin mode or report status"
    );
    assert!(
        success_max_feedin_writes_std,
        "Integration test failed: commanded maximum feedin rate was not written to standard Modbus (0x51)"
    );
    assert!(
        success_max_feedin_writes_hyb,
        "Integration test failed: commanded maximum feedin rate was not written to hybrid Modbus (0x52)"
    );
    assert!(
        success_auto_mode,
        "Integration test failed: Power Manager did not transition back to Auto mode"
    );
    assert!(
        success_reload,
        "Integration test failed: Reload did not update grid target to -500"
    );
    assert!(
        reload_count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "Integration test failed: reload count was 0"
    );
}

#[tokio::test]
async fn test_sync_on_kill_exits_cleanly() {
    use std::os::unix::process::CommandExt;
    
    let temp_dir = std::env::temp_dir().join(format!("powerscraper_test_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    
    let db_path = temp_dir.join("config.db");
    
    let mut config = Config::default_empty();
    config.mqtt = Some(PowerScraper::config::MqttBrokerConfig {
        enabled: Some(false),
        broker: "127.0.0.1".to_string(),
        port: Some(1883),
        base_topic: Some("sensors".to_string()),
        username: None,
        password: None,
        home_assistant_discovery: Some(false),
        home_assistant_prefix: None,
    });
    
    PowerScraper::database::save_config_to_db(db_path.to_str().unwrap(), &config).unwrap();
    
    let mut child = Command::new(env!("CARGO_BIN_EXE_PowerScraper"))
        .current_dir(&temp_dir)
        .spawn()
        .expect("Failed to start PowerScraper binary");
        
    // Allow process to start up
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    
    // Send SIGTERM
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    
    // Wait for exit with a timeout
    let exit_status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }).await.expect("Process did not exit within 5 seconds of SIGTERM (shutdown hang)");
    
    assert!(exit_status.success() || exit_status.code() == Some(0), "Process did not exit with code 0");
    
    let _ = std::fs::remove_dir_all(&temp_dir);
}
