use crate::config::{MQTTInverterDeviceConfig, MqttBrokerConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::{Event, Packet, QoS};
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

pub async fn run_mqtt_inverter_driver(
    inverter_name: String,
    config: MQTTInverterDeviceConfig,
    mqtt_config: MqttBrokerConfig,
    tx_telemetry: tokio::sync::mpsc::Sender<crate::dispatch_manager::TelemetryBatch>,
    cancel_token: CancellationToken,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());

    // Connect to the main local broker for publishing standard metrics
    let main_client_id = format!("powerscraper-mqtt-inverter-pub-{}", inverter_name);
    let (pub_client, mut pub_eventloop) = create_mqtt_client(&main_client_id, &mqtt_config);
    
    // Spawn task to poll main broker's eventloop
    let cancel_clone = cancel_token.clone();
    let inverter_name_clone = inverter_name.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => break,
                res = pub_eventloop.poll() => {
                    if let Err(e) = res {
                        eprintln!("Publisher client error for inverter [{}]: {}", inverter_name_clone, e);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    });

    // Determine target broker for subscribing (the custom one or fallback to main/local)
    let broker = config.broker.clone().unwrap_or_else(|| mqtt_config.broker.clone());
    let port = config.port.or(mqtt_config.port).unwrap_or(1883);
    let username = config.username.clone().or_else(|| mqtt_config.username.clone());
    let password = config.password.clone().or_else(|| mqtt_config.password.clone());

    let broker_config = MqttBrokerConfig {
        broker,
        port: Some(port),
        username,
        password,
        base_topic: Some(base_topic.clone()),
        home_assistant_discovery: mqtt_config.home_assistant_discovery,
        home_assistant_prefix: mqtt_config.home_assistant_prefix.clone(),
    };

    let sub_client_id = format!("powerscraper-mqtt-inverter-sub-{}", inverter_name);
    let (sub_client, mut sub_eventloop) = create_mqtt_client(&sub_client_id, &broker_config);

    // Build the mapping of topics to standard metric names
    let mut topics = Vec::new();
    if let Some(ref t) = config.topic_pv1_power { topics.push((t.clone(), "PV1 Power")); }
    if let Some(ref t) = config.topic_pv2_power { topics.push((t.clone(), "PV2 Power")); }
    if let Some(ref t) = config.topic_pv1_voltage { topics.push((t.clone(), "PV1 Voltage")); }
    if let Some(ref t) = config.topic_pv2_voltage { topics.push((t.clone(), "PV2 Voltage")); }
    if let Some(ref t) = config.topic_pv1_current { topics.push((t.clone(), "PV1 Current")); }
    if let Some(ref t) = config.topic_pv2_current { topics.push((t.clone(), "PV2 Current")); }
    if let Some(ref t) = config.topic_grid_voltage { topics.push((t.clone(), "Grid Voltage")); }
    if let Some(ref t) = config.topic_grid_current { topics.push((t.clone(), "Grid Current")); }
    if let Some(ref t) = config.topic_grid_power { topics.push((t.clone(), "Grid Power")); }
    if let Some(ref t) = config.topic_frequency { topics.push((t.clone(), "Frequency")); }
    if let Some(ref t) = config.topic_temperature { topics.push((t.clone(), "Inverter Temperature")); }
    if let Some(ref t) = config.topic_energy_today { topics.push((t.clone(), "Energy Today")); }
    if let Some(ref t) = config.topic_energy_total { topics.push((t.clone(), "Energy Total")); }
    if let Some(ref t) = config.topic_battery_capacity { topics.push((t.clone(), "Battery Capacity")); }
    if let Some(ref t) = config.topic_battery_power { topics.push((t.clone(), "Battery Power")); }

    // Subscribe to all configured topics
    let mut subscribed = false;
    for (topic, _) in &topics {
        if let Err(e) = sub_client.subscribe(topic, QoS::AtLeastOnce).await {
            println!(
                "MQTT Inverter [{}] failed to subscribe to topic '{}': {}",
                inverter_name, topic, e
            );
        } else {
            subscribed = true;
        }
    }

    if subscribed {
        println!(
            "MQTT Inverter [{}] successfully subscribed to configured topics.",
            inverter_name
        );
    } else {
        println!(
            "MQTT Inverter [{}] has no topics configured for subscription.",
            inverter_name
        );
    }

    // Seed status.inverters so the inverter appears in web UI immediately
    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
        let inv = status.inverters.entry(inverter_name.clone()).or_default();
        inv.run_mode = 2; // Normal running state
    }

    let mut pv1_power = 0.0;
    let mut pv2_power = 0.0;
    let mut pv1_voltage = 0.0;
    let mut pv1_current = 0.0;
    let mut pv2_voltage = 0.0;
    let mut pv2_current = 0.0;
    let mut battery_capacity = 0;
    let mut battery_power = 0.0;

    let mut discovered_metrics = std::collections::HashSet::new();

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            res = sub_eventloop.poll() => {
                match res {
                    Ok(notification) => {
                        if let Event::Incoming(Packet::Publish(publish)) = notification {
                            let payload = String::from_utf8_lossy(&publish.payload).trim().to_string();
                            
                            // Find matching topic
                            let mut matched_metric = None;
                            for (t, metric) in &topics {
                                if publish.topic == *t {
                                    matched_metric = Some(*metric);
                                    break;
                                }
                            }

                            if let Some(metric_name) = matched_metric {
                                // Parse value
                                let parsed_val: Option<f64> = payload.parse::<f64>().ok();

                                if let Some(val) = parsed_val {
                                     let mut metrics = std::collections::HashMap::new();
                                     metrics.insert(metric_name.to_string(), val);
                                     let batch = crate::dispatch_manager::TelemetryBatch {
                                         device_name: inverter_name.clone(),
                                         timestamp: chrono::Utc::now().timestamp(),
                                         metrics,
                                     };
                                     let _ = tx_telemetry.try_send(batch);

                                     if metric_name == "PV1 Power" {
                                        pv1_power = val;
                                    } else if metric_name == "PV2 Power" {
                                        pv2_power = val;
                                    } else if metric_name == "PV1 Voltage" {
                                        pv1_voltage = val;
                                    } else if metric_name == "PV1 Current" {
                                        pv1_current = val;
                                    } else if metric_name == "PV2 Voltage" {
                                        pv2_voltage = val;
                                    } else if metric_name == "PV2 Current" {
                                        pv2_current = val;
                                    } else if metric_name == "Battery Capacity" {
                                        battery_capacity = val.round() as u32;
                                    } else if metric_name == "Battery Power" {
                                        battery_power = val;
                                    }

                                    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                        let inv = status.inverters.entry(inverter_name.clone()).or_default();
                                        let total_pv = if (pv1_power + pv2_power) > 0.0 {
                                            pv1_power + pv2_power
                                        } else {
                                            (pv1_voltage * pv1_current) + (pv2_voltage * pv2_current)
                                        };
                                        inv.pv_power = total_pv.round() as u32;
                                        inv.battery_capacity = battery_capacity as u8;
                                        inv.battery_power = battery_power.round() as i32;
                                        inv.run_mode = 2; // Normal running state
                                        inv.last_updated = Some(std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs());
                                    }
                                }

                                // Publish HA auto-discovery (to main/local broker)
                                if !discovered_metrics.contains(metric_name) {
                                    crate::mqtt_helper::publish_home_assistant_discovery(
                                        &pub_client,
                                        &mqtt_config,
                                        &inverter_name,
                                        metric_name,
                                        false,
                                    )
                                    .await;
                                    discovered_metrics.insert(metric_name.to_string());
                                }

                                // Republish to main/local broker under the standard topic format
                                let target_topic = format!("{}/{}/{}", base_topic, inverter_name, metric_name);
                                if let Err(e) = pub_client
                                    .publish(&target_topic, QoS::AtMostOnce, false, payload)
                                    .await
                                    {
                                        println!(
                                            "MQTT Inverter [{}] failed to publish standard topic {}: {}",
                                            inverter_name, target_topic, e
                                        );
                                    }
                            }
                        }
                    }
                    Err(e) => {
                        println!("MQTT Inverter [{}] subscriber connection error: {}", inverter_name, e);
                        tokio::select! {
                            _ = cancel_token.cancelled() => break,
                            _ = sleep(Duration::from_secs(5)) => {}
                        }
                    }
                }
            }
        }
    }
}
