use crate::config::{MQTTPowerMeterDeviceConfig, MqttBrokerConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::{Event, Packet, QoS};
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

pub async fn run_mqtt_meter_driver(
    meter_name: String,
    config: MQTTPowerMeterDeviceConfig,
    mqtt_config: MqttBrokerConfig,
    tx_telemetry: tokio::sync::mpsc::Sender<crate::dispatch_manager::TelemetryBatch>,
    cancel_token: CancellationToken,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = format!("powerscraper-mqtt-meter-{}", meter_name);
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    let topic_total = config
        .topic_total
        .clone()
        .unwrap_or_else(|| "TotalSystemPower".to_string());
    let topic_p1 = config
        .topic_phase1
        .clone()
        .unwrap_or_else(|| "Phase1Power".to_string());
    let topic_p2 = config
        .topic_phase2
        .clone()
        .unwrap_or_else(|| "Phase2Power".to_string());
    let topic_p3 = config
        .topic_phase3
        .clone()
        .unwrap_or_else(|| "Phase3Power".to_string());

    let meter_topics = [
        topic_total.clone(),
        topic_p1.clone(),
        topic_p2.clone(),
        topic_p3.clone(),
    ];

    if let Err(e) = mqtt_client.subscribe(&topic_total, QoS::AtLeastOnce).await {
        println!(
            "MQTT Meter [{}] failed to subscribe to total topic: {}",
            meter_name, e
        );
    }
    let _ = mqtt_client.subscribe(&topic_p1, QoS::AtLeastOnce).await;
    let _ = mqtt_client.subscribe(&topic_p2, QoS::AtLeastOnce).await;
    let _ = mqtt_client.subscribe(&topic_p3, QoS::AtLeastOnce).await;

    println!(
        "MQTT Meter [{}] bridged custom topics -> standard base topic '{}'",
        meter_name, base_topic
    );

    let mut discovered_metrics = std::collections::HashSet::new();

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            res = eventloop.poll() => {
                match res {
                    Ok(notification) => {
                        match notification {
                            Event::Incoming(Packet::ConnAck(_)) => {
                                for t in &meter_topics {
                                    if let Err(e) = mqtt_client.subscribe(t, QoS::AtLeastOnce).await {
                                        eprintln!(
                                            "MQTT Meter [{}] failed to re-subscribe to topic '{}' on ConnAck: {}",
                                            meter_name, t, e
                                        );
                                    }
                                }
                            }
                            Event::Incoming(Packet::Publish(publish)) => {
                                let payload = String::from_utf8_lossy(&publish.payload).trim().to_string();
                                let target_metric = if publish.topic == topic_total {
                                    Some("Total system power")
                                } else if publish.topic == topic_p1 {
                                    Some("Phase 1 power")
                                } else if publish.topic == topic_p2 {
                                    Some("Phase 2 power")
                                } else if publish.topic == topic_p3 {
                                    Some("Phase 3 power")
                                } else {
                                    None
                                };

                                if let Some(metric_name) = target_metric {
                                    if let Ok(num) = payload.parse::<f64>() {
                                        let mut metrics = std::collections::HashMap::new();
                                        metrics.insert(metric_name.to_string(), num);
                                        let batch = crate::dispatch_manager::TelemetryBatch {
                                            device_name: meter_name.clone(),
                                            timestamp: chrono::Utc::now().timestamp(),
                                            metrics,
                                        };
                                        let _ = tx_telemetry.try_send(batch);
                                    }

                                    if !discovered_metrics.contains(metric_name) {
                                        crate::mqtt_helper::publish_home_assistant_discovery(
                                            &mqtt_client,
                                            &mqtt_config,
                                            &meter_name,
                                            metric_name,
                                            false,
                                        )
                                        .await;
                                        discovered_metrics.insert(metric_name.to_string());
                                    }

                                    let target_topic = format!("{}/{}/{}", base_topic, meter_name, metric_name);
                                    if let Err(e) = mqtt_client
                                        .publish(&target_topic, QoS::AtMostOnce, false, payload)
                                        .await
                                    {
                                        println!(
                                            "MQTT Meter [{}] failed to publish standard topic {}: {}",
                                            meter_name, target_topic, e
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        println!("MQTT Meter [{}] connection error: {}", meter_name, e);
                        tokio::select! {
                            _ = cancel_token.cancelled() => break,
                            _ = sleep(Duration::from_millis(500)) => {}
                        }
                    }
                }
            }
        }
    }
}
