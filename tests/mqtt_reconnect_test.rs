use PowerScraper::config::{MQTTInverterDeviceConfig, MQTTPowerMeterDeviceConfig, MqttBrokerConfig};
use PowerScraper::dispatch_manager::DriverCommand;
use PowerScraper::drivers;
use PowerScraper::forwarders::MQTTForwarder;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::fs::File;
use std::io::Write;
use std::process::{Child, Command};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

struct MosquittoServer {
    child: Child,
    conf_path: String,
    mosquitto_bin: String,
    _port: u16,
}

impl MosquittoServer {
    fn start(port: u16) -> Self {
        let _ = std::fs::create_dir_all("target");
        let conf_content = format!("listener {}\nallow_anonymous true\n", port);
        let conf_path = format!("target/mosquitto_test_{}.conf", port);
        {
            let mut file = File::create(&conf_path).expect("Failed to create mosquitto conf");
            file.write_all(conf_content.as_bytes())
                .expect("Failed to write mosquitto conf");
            file.flush().expect("Failed to flush mosquitto conf");
            file.sync_all().expect("Failed to sync mosquitto conf");
        }

        let mosquitto_bin = format!("target/mosquitto_bin_{}", port);
        if let Err(e) = std::fs::copy("/usr/sbin/mosquitto", &mosquitto_bin) {
            eprintln!("Warning: Failed to copy mosquitto to {}: {}", mosquitto_bin, e);
        }

        let child = Command::new(&mosquitto_bin)
            .arg("-c")
            .arg(&conf_path)
            .spawn()
            .or_else(|_| {
                Command::new("mosquitto").arg("-c").arg(&conf_path).spawn()
            })
            .expect("Failed to start mosquitto broker");

        MosquittoServer {
            child,
            conf_path,
            mosquitto_bin,
            _port: port,
        }
    }

    fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(300));
        self.child = Command::new(&self.mosquitto_bin)
            .arg("-c")
            .arg(&self.conf_path)
            .spawn()
            .or_else(|_| {
                Command::new("mosquitto").arg("-c").arg(&self.conf_path).spawn()
            })
            .expect("Failed to restart mosquitto broker");
    }
}

impl Drop for MosquittoServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.conf_path);
        let _ = std::fs::remove_file(&self.mosquitto_bin);
    }
}

async fn get_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn create_publisher(client_id: &str, port: u16) -> (AsyncClient, rumqttc::EventLoop) {
    let mut options = MqttOptions::new(client_id, "127.0.0.1", port);
    options.set_keep_alive(Duration::from_secs(10));
    AsyncClient::new(options, 100)
}

#[tokio::test]
async fn test_mqtt_inverter_status_and_reconnect() {
    let port = get_free_port().await;
    let mut broker = MosquittoServer::start(port);
    sleep(Duration::from_millis(600)).await;

    let mqtt_config = MqttBrokerConfig {
        enabled: Some(true),
        broker: "127.0.0.1".to_string(),
        port: Some(port),
        username: None,
        password: None,
        base_topic: Some("sensors".to_string()),
        home_assistant_discovery: Some(false),
        home_assistant_prefix: None,
    };

    let inverter_name = "test-inv".to_string();
    let inv_config = MQTTInverterDeviceConfig {
        broker: None,
        port: None,
        username: None,
        password: None,
        topic_pv1_power: Some("test_inv/pv1".to_string()),
        topic_pv2_power: None,
        topic_pv1_voltage: None,
        topic_pv2_voltage: None,
        topic_pv1_current: None,
        topic_pv2_current: None,
        topic_grid_voltage: None,
        topic_grid_current: None,
        topic_grid_power: None,
        topic_frequency: None,
        topic_temperature: None,
        topic_energy_today: None,
        topic_energy_total: None,
        topic_battery_capacity: Some("test_inv/soc".to_string()),
        topic_battery_power: Some("test_inv/bat_power".to_string()),
    };

    let (tx_telemetry, mut rx_telemetry) = tokio::sync::mpsc::channel(100);
    let cancel_token = CancellationToken::new();

    let inv_name_clone = inverter_name.clone();
    let inv_cfg_clone = inv_config.clone();
    let mqtt_cfg_clone = mqtt_config.clone();
    let cancel_clone = cancel_token.clone();

    tokio::spawn(async move {
        drivers::mqtt_inverter::run_mqtt_inverter_driver(
            inv_name_clone,
            inv_cfg_clone,
            mqtt_cfg_clone,
            tx_telemetry,
            cancel_clone,
        )
        .await;
    });

    sleep(Duration::from_millis(600)).await;

    // Verify initial status entry has driver_type set
    {
        let status = PowerScraper::web_server::get_system_status().lock().unwrap();
        let inv = status.inverters.get(&inverter_name).expect("Inverter should be seeded in status");
        assert_eq!(inv.driver_type.as_deref(), Some("MQTTInverter"));
    }

    // Connect test publisher
    let (test_client, mut test_eventloop) = create_publisher("test-pub-inverter-1", port);
    tokio::spawn(async move {
        while let Ok(_) = test_eventloop.poll().await {}
    });

    // 1. Publish initial metrics
    test_client
        .publish("test_inv/pv1", QoS::AtLeastOnce, false, "3450.0")
        .await
        .unwrap();
    test_client
        .publish("test_inv/soc", QoS::AtLeastOnce, false, "88")
        .await
        .unwrap();
    test_client
        .publish("test_inv/bat_power", QoS::AtLeastOnce, false, "-1250.0")
        .await
        .unwrap();

    // Verify telemetry received
    let mut received_metrics = std::collections::HashMap::new();
    for _ in 0..15 {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(500), rx_telemetry.recv()).await {
            for (k, v) in batch.metrics {
                received_metrics.insert(k, v);
            }
            if received_metrics.len() >= 3 {
                break;
            }
        }
    }

    assert_eq!(received_metrics.get("PV1 Power"), Some(&3450.0));
    assert_eq!(received_metrics.get("Battery Capacity"), Some(&88.0));
    assert_eq!(received_metrics.get("Battery Power"), Some(&-1250.0));

    // Verify status struct consistency
    {
        let status = PowerScraper::web_server::get_system_status().lock().unwrap();
        let inv = status.inverters.get(&inverter_name).unwrap();
        assert_eq!(inv.driver_type.as_deref(), Some("MQTTInverter"));
        assert_eq!(inv.pv_power, 3450);
        assert_eq!(inv.battery_capacity, 88);
        assert_eq!(inv.battery_power, -1250);
        assert_eq!(inv.raw_metrics.get("PV1 Power").map(|s| s.as_str()), Some("3450.0"));
        assert_eq!(inv.raw_metrics.get("Battery Capacity").map(|s| s.as_str()), Some("88"));
        assert_eq!(inv.raw_metrics.get("Battery Power").map(|s| s.as_str()), Some("-1250.0"));
    }

    // 2. Restart Mosquitto broker (simulating network bounce / broker restart)
    println!("Restarting Mosquitto broker to test ConnAck re-subscription...");
    broker.restart();
    sleep(Duration::from_millis(1500)).await;

    // Connect new test publisher after broker restart
    let (test_client2, mut test_eventloop2) = create_publisher("test-pub-inverter-2", port);
    tokio::spawn(async move {
        while let Ok(_) = test_eventloop2.poll().await {}
    });
    sleep(Duration::from_millis(500)).await;

    // Publish new metrics after reconnect in a loop until received
    let mut post_reconnect_pv1 = None;
    for _ in 0..20 {
        let _ = test_client2
            .publish("test_inv/pv1", QoS::AtLeastOnce, false, "4500.0")
            .await;
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(300), rx_telemetry.recv()).await {
            if let Some(val) = batch.metrics.get("PV1 Power") {
                if *val == 4500.0 {
                    post_reconnect_pv1 = Some(*val);
                    break;
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }

    assert_eq!(
        post_reconnect_pv1,
        Some(4500.0),
        "MQTT Inverter driver failed to receive messages after broker reconnection"
    );

    // Verify status struct updated after reconnect
    {
        let status = PowerScraper::web_server::get_system_status().lock().unwrap();
        let inv = status.inverters.get(&inverter_name).unwrap();
        assert_eq!(inv.pv_power, 4500);
        assert_eq!(inv.raw_metrics.get("PV1 Power").map(|s| s.as_str()), Some("4500.0"));
    }

    cancel_token.cancel();
}

#[tokio::test]
async fn test_mqtt_meter_reconnect() {
    let port = get_free_port().await;
    let mut broker = MosquittoServer::start(port);
    sleep(Duration::from_millis(600)).await;

    let mqtt_config = MqttBrokerConfig {
        enabled: Some(true),
        broker: "127.0.0.1".to_string(),
        port: Some(port),
        username: None,
        password: None,
        base_topic: Some("sensors".to_string()),
        home_assistant_discovery: Some(false),
        home_assistant_prefix: None,
    };

    let meter_config = MQTTPowerMeterDeviceConfig {
        broker: "127.0.0.1".to_string(),
        port: Some(port),
        topic_total: Some("meter/total".to_string()),
        topic_phase1: Some("meter/p1".to_string()),
        topic_phase2: None,
        topic_phase3: None,
        username: None,
        password: None,
        poll_period: None,
        watchdog_timeout: None,
    };

    let (tx_telemetry, mut rx_telemetry) = tokio::sync::mpsc::channel(100);
    let cancel_token = CancellationToken::new();

    let meter_cfg_clone = meter_config.clone();
    let mqtt_cfg_clone = mqtt_config.clone();
    let cancel_clone = cancel_token.clone();

    tokio::spawn(async move {
        drivers::mqtt_meter::run_mqtt_meter_driver(
            "custom-meter".to_string(),
            meter_cfg_clone,
            mqtt_cfg_clone,
            tx_telemetry,
            cancel_clone,
        )
        .await;
    });

    sleep(Duration::from_millis(600)).await;

    let (test_client, mut test_eventloop) = create_publisher("test-pub-meter-1", port);
    tokio::spawn(async move {
        while let Ok(_) = test_eventloop.poll().await {}
    });

    test_client
        .publish("meter/total", QoS::AtLeastOnce, false, "2500.0")
        .await
        .unwrap();

    let mut initial_val = None;
    for _ in 0..15 {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(500), rx_telemetry.recv()).await {
            if let Some(v) = batch.metrics.get("Total system power") {
                initial_val = Some(*v);
                break;
            }
        }
    }
    assert_eq!(initial_val, Some(2500.0));

    // Restart broker
    println!("Restarting Mosquitto broker for meter re-subscription test...");
    broker.restart();
    sleep(Duration::from_millis(1500)).await;

    let (test_client2, mut test_eventloop2) = create_publisher("test-pub-meter-2", port);
    tokio::spawn(async move {
        while let Ok(_) = test_eventloop2.poll().await {}
    });
    sleep(Duration::from_millis(500)).await;

    let mut post_reconnect_val = None;
    for _ in 0..20 {
        let _ = test_client2
            .publish("meter/total", QoS::AtLeastOnce, false, "3100.0")
            .await;
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(300), rx_telemetry.recv()).await {
            if let Some(v) = batch.metrics.get("Total system power") {
                if *v == 3100.0 {
                    post_reconnect_val = Some(*v);
                    break;
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }

    assert_eq!(
        post_reconnect_val,
        Some(3100.0),
        "MQTT Meter driver failed to receive messages after broker reconnection"
    );

    cancel_token.cancel();
}

#[tokio::test]
async fn test_mqtt_forwarder_reconnect() {
    let port = get_free_port().await;
    let mut broker = MosquittoServer::start(port);
    sleep(Duration::from_millis(600)).await;

    let mqtt_config = MqttBrokerConfig {
        enabled: Some(true),
        broker: "127.0.0.1".to_string(),
        port: Some(port),
        username: None,
        password: None,
        base_topic: Some("sensors".to_string()),
        home_assistant_discovery: Some(false),
        home_assistant_prefix: None,
    };

    let (_tx_input, rx_input) = tokio::sync::mpsc::channel(100);
    let (cmd_sender, mut cmd_receiver) = tokio::sync::mpsc::channel(100);
    let cancel_token = CancellationToken::new();

    let forwarder = MQTTForwarder::new(mqtt_config.clone(), rx_input, cmd_sender);
    let cancel_clone = cancel_token.clone();

    tokio::spawn(async move {
        forwarder.run(cancel_clone).await;
    });

    sleep(Duration::from_millis(600)).await;

    let (test_client, mut test_eventloop) = create_publisher("test-pub-fwd-1", port);
    tokio::spawn(async move {
        while let Ok(_) = test_eventloop.poll().await {}
    });

    let mut received_cmd = None;
    for _ in 0..20 {
        let _ = test_client
            .publish("sensors/power_manager/command/mode", QoS::AtLeastOnce, false, "ChargeBatteries")
            .await;
        if let Ok(Some(cmd)) = tokio::time::timeout(Duration::from_millis(300), cmd_receiver.recv()).await {
            received_cmd = Some(cmd);
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    match received_cmd {
        Some(DriverCommand::SetMode { mode }) => assert_eq!(mode, "ChargeBatteries"),
        other => panic!("Expected SetMode ChargeBatteries, got {:?}", other),
    }

    // Restart broker
    println!("Restarting Mosquitto broker for forwarder re-subscription test...");
    broker.restart();
    sleep(Duration::from_millis(1500)).await;

    let (test_client2, mut test_eventloop2) = create_publisher("test-pub-fwd-2", port);
    tokio::spawn(async move {
        while let Ok(_) = test_eventloop2.poll().await {}
    });
    sleep(Duration::from_millis(500)).await;

    let mut post_reconnect_cmd = None;
    for _ in 0..20 {
        let _ = test_client2
            .publish("sensors/power_manager/command/mode", QoS::AtLeastOnce, false, "Auto")
            .await;
        if let Ok(Some(cmd)) = tokio::time::timeout(Duration::from_millis(300), cmd_receiver.recv()).await {
            post_reconnect_cmd = Some(cmd);
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    match post_reconnect_cmd {
        Some(DriverCommand::SetMode { mode }) => assert_eq!(mode, "Auto"),
        other => panic!("Expected SetMode Auto after broker reconnect, got {:?}", other),
    }

    cancel_token.cancel();
}
