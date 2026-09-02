use crate::config::MqttBrokerConfig;
use crate::dispatch_manager::{DriverCommand, TelemetryBatch};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::{Event, Packet, QoS};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

pub struct MQTTForwarder {
    mqtt_config: MqttBrokerConfig,
    rx_input: mpsc::Receiver<TelemetryBatch>,
    cmd_sender: mpsc::Sender<DriverCommand>,
}

impl MQTTForwarder {
    pub fn new(
        mqtt_config: MqttBrokerConfig,
        rx_input: mpsc::Receiver<TelemetryBatch>,
        cmd_sender: mpsc::Sender<DriverCommand>,
    ) -> Self {
        MQTTForwarder {
            mqtt_config,
            rx_input,
            cmd_sender,
        }
    }

    pub async fn run(mut self, cancel_token: CancellationToken) {
        let base_topic = self
            .mqtt_config
            .base_topic
            .clone()
            .unwrap_or_else(|| "sensors".to_string());
        let (client, mut eventloop) = create_mqtt_client("powerscraper-mqtt-forwarder", &self.mqtt_config);

        // Subscribe to command topic
        let command_wildcard = format!("{}/power_manager/command/#", base_topic);
        if let Err(e) = client.subscribe(&command_wildcard, QoS::AtLeastOnce).await {
            println!("MQTT Forwarder failed to subscribe to {}: {}", command_wildcard, e);
        } else {
            println!("MQTT Forwarder subscribed to command topic: {}", command_wildcard);
        }

        // Publish HA Discovery for known static metrics & entities
        let discovery_enabled = self.mqtt_config.home_assistant_discovery.unwrap_or(true);
        if discovery_enabled {
            let static_discoveries = [
                ("power_manager", "mode"),
                ("power_manager", "grid_target"),
                ("aggregate", "Total Solar Production"),
                ("aggregate", "Total Grid Power Used for Charging"),
                ("aggregate", "Total Consumption"),
                ("aggregate", "Total Charging"),
                ("aggregate", "Total Discharging"),
                ("aggregate", "Power Budget"),
                ("aggregate", "Power Budget with Charging"),
                ("aggregate", "Stored Battery Unit Cost"),
                ("aggregate", "Battery Stored Energy"),
                ("aggregate", "Total Battery SoC"),
                ("tariff", "import_price"),
                ("tariff", "export_price"),
                ("amber", "import_price"),
                ("amber", "export_price"),
                ("amber", "general_price"),
                ("amber", "feedin_price"),
            ];
            for (device, metric) in &static_discoveries {
                crate::mqtt_helper::publish_home_assistant_discovery(
                    &client,
                    &self.mqtt_config,
                    device,
                    metric,
                    false,
                )
                .await;
            }
        }

        let announced_metrics: Arc<Mutex<HashSet<(String, String)>>> = Arc::new(Mutex::new(HashSet::new()));
        let mut currently_connected = false;
        let enqueue_wrapper = crate::mqtt_helper::MqttEnqueueWrapper::new();
        let _worker = enqueue_wrapper.spawn_worker(client.clone());

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    println!("MQTT Forwarder shutting down...");
                    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                        status.mqtt_connected = false;
                    }
                    break;
                }
                Some(batch) = self.rx_input.recv() => {
                    for (metric_name, val) in batch.metrics {
                        let topic = format!("{}/{}/{}", base_topic, batch.device_name, metric_name);
                        let payload_str = val.to_string();
                        enqueue_wrapper.enqueue(&topic, QoS::AtLeastOnce, false, payload_str);

                        if discovery_enabled {
                            let key = (batch.device_name.clone(), metric_name.clone());
                            let mut lock = announced_metrics.lock().await;
                            if !lock.contains(&key) {
                                lock.insert(key);
                                drop(lock);
                                crate::mqtt_helper::publish_home_assistant_discovery(
                                    &client,
                                    &self.mqtt_config,
                                    &batch.device_name,
                                    &metric_name,
                                    false,
                                )
                                .await;
                            }
                        }
                    }
                }
                notification = eventloop.poll() => {
                    match notification {
                        Ok(evt) => {
                            match evt {
                                Event::Incoming(Packet::ConnAck(_)) => {
                                    if !currently_connected {
                                        currently_connected = true;
                                        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                            status.mqtt_connected = true;
                                        }
                                    }
                                    if let Err(e) = client.subscribe(&command_wildcard, QoS::AtLeastOnce).await {
                                        eprintln!("MQTT Forwarder failed to re-subscribe to {} on ConnAck: {}", command_wildcard, e);
                                    }
                                }
                                Event::Incoming(Packet::Publish(publish)) => {
                                    if !currently_connected {
                                        currently_connected = true;
                                        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                            status.mqtt_connected = true;
                                        }
                                    }
                                    let topic = publish.topic.clone();
                                    if let Ok(payload_str) = String::from_utf8(publish.payload.to_vec()) {
                                        if topic.ends_with("/mode") {
                                            let _ = self.cmd_sender.try_send(DriverCommand::SetMode { mode: payload_str.trim().to_string() });
                                        } else if topic.ends_with("/grid_target") {
                                            if let Ok(val) = payload_str.trim().parse::<f64>() {
                                                let _ = self.cmd_sender.try_send(DriverCommand::SetGridTarget { watts: val });
                                            }
                                        }
                                    }
                                }
                                _ => {
                                    if !currently_connected {
                                        currently_connected = true;
                                        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                            status.mqtt_connected = true;
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            if currently_connected {
                                currently_connected = false;
                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                    status.mqtt_connected = false;
                                }
                            }
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            }
        }
    }
}
