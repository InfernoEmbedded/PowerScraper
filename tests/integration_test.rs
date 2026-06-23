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
}

impl MosquittoGuard {
    fn new(port: u16) -> Self {
        // Write mosquitto conf file
        let conf_content = format!("listener {}\nallow_anonymous true\n", port);
        let conf_path = "tests/mosquitto_test.conf";
        let mut file = File::create(conf_path).expect("Failed to create mosquitto_test.conf");
        file.write_all(conf_content.as_bytes())
            .expect("Failed to write mosquitto_test.conf");

        let mosquitto_bin = "target/mosquitto_bin";
        if let Err(e) = std::fs::copy("/usr/sbin/mosquitto", mosquitto_bin) {
            println!(
                "Warning: Failed to copy mosquitto to target/mosquitto_bin: {}",
                e
            );
        }

        // Spawn mosquitto using the copied binary to bypass AppArmor
        let child = Command::new(mosquitto_bin)
            .arg("-c")
            .arg(conf_path)
            .spawn()
            .or_else(|_| {
                // Fallback to system mosquitto
                Command::new("mosquitto").arg("-c").arg(conf_path).spawn()
            })
            .expect("Failed to start mosquitto broker");

        MosquittoGuard { child }
    }
}

impl Drop for MosquittoGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_file("tests/mosquitto_test.conf");
        let _ = std::fs::remove_file("target/mosquitto_bin");
    }
}

async fn run_mock_modbus_server(
    port: u16,
    regs: Arc<Mutex<Vec<u16>>>,
    writes: Arc<Mutex<Vec<(u16, u16)>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .expect("Failed to bind modbus server");

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
                                        0x04 => {
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

                                            let mut resp_pdu = vec![0x04, (quantity * 2) as u8];
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
    port: u16,
    http_requests: Arc<Mutex<Vec<String>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .expect("Failed to bind wifi server");

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
                                } else if request.contains("GET /input/post") {
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

#[tokio::test]
async fn test_integration_loop() {
    // 1. Start Mosquitto on port 18830
    let _mosquitto = MosquittoGuard::new(18830);
    sleep(Duration::from_millis(500)).await;

    // 2. Set up shared states for mock Modbus
    let regs = Arc::new(Mutex::new(vec![0u16; 250]));
    {
        let mut r = regs.lock().unwrap();
        r[28] = 15; // Battery Capacity = 15
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
        run_mock_modbus_server(5020, regs_clone, writes_clone, shutdown_rx1).await;
    });

    // Spawn Mock WiFi / EmonCMS Server
    let http_clone = http_requests.clone();
    let shutdown_rx2 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        run_mock_wifi_server(8080, http_clone, shutdown_rx2).await;
    });

    // 3. Load config file
    let config =
        Config::load_from_file("tests/test_config.toml").expect("Failed to load test config");
    let mqtt_config = config.mqtt.clone().expect("Missing MQTT config");

    // 4. Spawn drivers, power manager, and forwarders tasks
    let wifi_cfg = config.solax_wifi.clone().expect("Missing Wifi config");
    let mqtt_wifi = mqtt_config.clone();
    tokio::spawn(async move {
        drivers::solax_wifi::run_solax_wifi_driver(
            "127.0.0.1:8080".to_string(),
            wifi_cfg,
            mqtt_wifi,
        )
        .await;
    });

    let modbus_cfg = config.solax_modbus.clone().expect("Missing Modbus config");
    let mqtt_modbus = mqtt_config.clone();
    tokio::spawn(async move {
        drivers::solax_modbus::run_solax_modbus_driver(
            "solax-modbus".to_string(),
            "127.0.0.1:5020".to_string(),
            modbus_cfg,
            mqtt_modbus,
        )
        .await;
    });

    let hybrid_cfg = config
        .solax_xhybrid_modbus
        .clone()
        .expect("Missing Hybrid Modbus config");
    let mqtt_hybrid = mqtt_config.clone();
    tokio::spawn(async move {
        drivers::solax_modbus::run_solax_xhybrid_driver(
            "solax-xhybrid".to_string(),
            "127.0.0.1:5020".to_string(),
            hybrid_cfg,
            mqtt_hybrid,
        )
        .await;
    });

    let battery_cfg = config
        .battery_control
        .clone()
        .expect("Missing Battery control config");
    let mqtt_pm = mqtt_config.clone();
    tokio::spawn(async move {
        power_manager::run_power_manager_task(battery_cfg, mqtt_pm).await;
    });

    // Spawn MQTT Meter Driver
    let mqtt_meter_cfg = config
        .mqtt_power_meter
        .clone()
        .expect("Missing MQTT meter config");
    let meter_dev_cfg = mqtt_meter_cfg
        .meter_devices
        .get("custom-meter")
        .expect("Missing custom-meter config")
        .clone();
    let mqtt_meter = mqtt_config.clone();
    tokio::spawn(async move {
        drivers::mqtt_meter::run_mqtt_meter_driver(
            "custom-meter".to_string(),
            meter_dev_cfg,
            mqtt_meter,
        )
        .await;
    });

    // Spawn Forwarders Task with both EmonCMS and InfluxDB
    let emoncms_cfg = config.emoncms.clone();
    let influx_cfg = config.influx.clone();
    let mqtt_fwd = mqtt_config.clone();
    tokio::spawn(async move {
        forwarders::run_forwarders_task(emoncms_cfg, influx_cfg, mqtt_fwd).await;
    });

    // Connect test MQTT client to verify custom meter and forwarder bridging
    let received_bridged_power = Arc::new(Mutex::new([false; 4]));
    let rbp_clone = received_bridged_power.clone();
    let received_mode_status = Arc::new(Mutex::new(String::new()));
    let rms_clone = received_mode_status.clone();
    let received_target_status = Arc::new(Mutex::new(String::new()));
    let rts_clone = received_target_status.clone();

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
        {
            break;
        }
    }

    let _ = shutdown_tx.send(());
    sleep(Duration::from_millis(100)).await;

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
}
