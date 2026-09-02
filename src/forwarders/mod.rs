pub mod emoncms;
pub mod influx;
pub mod mqtt_forwarder;

pub use emoncms::EmonCMSForwarder;
pub use influx::InfluxForwarder;
pub use mqtt_forwarder::MQTTForwarder;

use crate::config::{EmonCMSConfig, InfluxConfig, MqttBrokerConfig};
use tokio_util::sync::CancellationToken;

pub async fn run_forwarders_task(
    emoncms_config: Option<EmonCMSConfig>,
    influx_config: Option<InfluxConfig>,
    mqtt_config: MqttBrokerConfig,
    cancel_token: CancellationToken,
) {
    if emoncms_config.is_none() && influx_config.is_none() {
        return;
    }

    let mut senders = Vec::new();

    if let Some(emon) = emoncms_config {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::dispatch_manager::TelemetryBatch>(2048);
        senders.push(tx);
        let fwd = EmonCMSForwarder::new(emon, rx);
        let token = cancel_token.clone();
        tokio::spawn(async move {
            fwd.run(token).await;
        });
    }

    if let Some(influx) = influx_config {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::dispatch_manager::TelemetryBatch>(2048);
        senders.push(tx);
        let fwd = InfluxForwarder::new(influx, rx);
        let token = cancel_token.clone();
        tokio::spawn(async move {
            fwd.run(token).await;
        });
    }

    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let (mqtt_client, mut eventloop) = crate::mqtt_helper::create_mqtt_client("powerscraper-forwarder-legacy", &mqtt_config);

    let status_wildcard = format!("{}/#", base_topic);
    let _ = mqtt_client.subscribe(&status_wildcard, rumqttc::QoS::AtLeastOnce).await;

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            notification = eventloop.poll() => {
                match notification {
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                        let _ = mqtt_client.subscribe(&status_wildcard, rumqttc::QoS::AtLeastOnce).await;
                    }
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                        let topic_suffix = publish.topic.strip_prefix(&format!("{}/", base_topic));
                        if let Some(suffix) = topic_suffix {
                            let parts: Vec<&str> = suffix.split('/').collect();
                            if parts.len() >= 2 {
                                let device_name = parts[0].to_string();
                                let metric = parts[1..].join("/");
                                let payload = String::from_utf8_lossy(&publish.payload).trim().to_string();
                                if let Ok(val) = payload.parse::<f64>() {
                                    let mut metrics = std::collections::HashMap::new();
                                    metrics.insert(metric, val);
                                    let batch = crate::dispatch_manager::TelemetryBatch {
                                        device_name,
                                        timestamp: chrono::Utc::now().timestamp(),
                                        metrics,
                                    };
                                    for sender in &senders {
                                        let _ = sender.try_send(batch.clone());
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
