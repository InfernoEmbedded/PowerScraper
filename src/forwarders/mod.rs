use crate::config::{EmonCMSConfig, InfluxConfig, MqttBrokerConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::{Event, Packet, QoS};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

pub mod emoncms;
pub mod influx;

pub async fn run_forwarders_task(
    emoncms_config: Option<EmonCMSConfig>,
    influx_config: Option<InfluxConfig>,
    mqtt_config: MqttBrokerConfig,
    cancel_token: CancellationToken,
) {
    if emoncms_config.is_none() && influx_config.is_none() {
        return;
    }

    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = "powerscraper-forwarder";
    let (mqtt_client, mut eventloop) = create_mqtt_client(client_id, &mqtt_config);

    // Buffer to hold latest metrics per device
    // Structure: device_name -> (metric_key -> metric_value)
    let buffer = Arc::new(Mutex::new(HashMap::<String, HashMap<String, String>>::new()));

    // Subscribe to all status topics
    let status_wildcard = format!("{}/#", base_topic);
    if let Err(e) = mqtt_client
        .subscribe(&status_wildcard, QoS::AtLeastOnce)
        .await
    {
        println!("Forwarder failed to subscribe: {}", e);
        return;
    }

    println!(
        "Forwarder task running and subscribed to: {}",
        status_wildcard
    );

    // Spawn the periodic flush loop
    let buffer_clone = buffer.clone();
    let emon_clone = emoncms_config.clone();
    let influx_clone = influx_config.clone();
    let cancel_token_flush = cancel_token.clone();
    tokio::spawn(async move {
        let http_client = reqwest::Client::new();
        #[cfg(test)]
        let flush_interval = Duration::from_secs(1);
        #[cfg(not(test))]
        let flush_interval = Duration::from_secs(10);

        loop {
            tokio::select! {
                _ = cancel_token_flush.cancelled() => break,
                _ = sleep(flush_interval) => {}
            }

            let mut data_to_flush = HashMap::new();
            {
                let mut buf_lock = buffer_clone.lock().await;
                // Swap buffer to release lock quickly
                std::mem::swap(&mut *buf_lock, &mut data_to_flush);
            }

            if data_to_flush.is_empty() {
                continue;
            }

            for (device_name, metrics) in data_to_flush {
                if metrics.is_empty() {
                    continue;
                }

                // 1. Flush to EmonCMS
                if let Some(ref emon) = emon_clone {
                    emoncms::forward_to_emoncms(&http_client, emon, &device_name, &metrics, &cancel_token_flush).await;
                }

                // 2. Flush to InfluxDB v2
                if let Some(ref influx) = influx_clone {
                    influx::forward_to_influx(&http_client, influx, &device_name, &metrics, &cancel_token_flush).await;
                }
            }
        }
    });

    // Main MQTT subscription polling loop
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            res = eventloop.poll() => {
                match res {
                    Ok(notification) => {
                        if let Event::Incoming(Packet::Publish(publish)) = notification {
                            let topic_suffix = publish.topic.strip_prefix(&format!("{}/", base_topic));
                            if let Some(suffix) = topic_suffix {
                                let parts: Vec<&str> = suffix.split('/').collect();
                                if parts.len() >= 2 {
                                    let device_name = parts[0];
                                    let metric = parts[1..].join("/");
                                    let payload =
                                        String::from_utf8_lossy(&publish.payload).trim().to_string();

                                    // Buffer the metric
                                    let mut buf_lock = buffer.lock().await;
                                    buf_lock
                                        .entry(device_name.to_string())
                                        .or_default()
                                        .insert(metric, payload);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("Forwarder MQTT error: {}", e);
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
