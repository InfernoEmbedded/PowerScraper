use crate::config::{EmonCMSConfig, InfluxConfig, MqttBrokerConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::{Event, Packet, QoS};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

pub async fn run_forwarders_task(
    emoncms_config: Option<EmonCMSConfig>,
    influx_config: Option<InfluxConfig>,
    mqtt_config: MqttBrokerConfig,
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
    tokio::spawn(async move {
        let http_client = reqwest::Client::new();
        #[cfg(test)]
        let flush_interval = Duration::from_secs(1);
        #[cfg(not(test))]
        let flush_interval = Duration::from_secs(10);

        loop {
            sleep(flush_interval).await;

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
                    let mut payload = metrics.clone();
                    payload.remove("Serial");
                    payload.remove("name");

                    let url = format!("{}/input/post", emon.server);
                    let mut query_params = HashMap::new();
                    query_params.insert("apikey", emon.api_key.clone());
                    query_params.insert("node", device_name.clone());
                    if let Ok(json_str) = serde_json::to_string(&payload) {
                        query_params.insert("fulljson", json_str);

                        let emon_url = url.clone();
                        let client = http_client.clone();
                        let timeout_sec = emon.timeout;
                        tokio::spawn(async move {
                            match client
                                .get(&emon_url)
                                .query(&query_params)
                                .timeout(Duration::from_secs(timeout_sec))
                                .send()
                                .await
                            {
                                Ok(resp) => {
                                    if !resp.status().is_success() {
                                        println!(
                                            "EmonCMS forward failed with status: {}",
                                            resp.status()
                                        );
                                    }
                                }
                                Err(e) => {
                                    println!("EmonCMS request error: {}", e);
                                }
                            }
                        });
                    }
                }

                // 2. Flush to InfluxDB v2
                if let Some(ref influx) = influx_clone {
                    let mut payload = metrics.clone();
                    payload.remove("Serial");
                    payload.remove("name");

                    // Format as line protocol: solax,inverter=dev_name field1=val1,field2=val2
                    let mut fields = Vec::new();
                    for (k, v) in payload {
                        // sanitize key / value
                        let key = k.replace(' ', "_").replace(',', "\\,").replace('=', "\\=");
                        if let Ok(num) = v.parse::<f64>() {
                            fields.push(format!("{}={}", key, num));
                        } else {
                            let escaped_val = v.replace('"', "\\\"");
                            fields.push(format!("{}=\"{}\"", key, escaped_val));
                        }
                    }

                    if !fields.is_empty() {
                        let line = format!("solax,inverter={} {}", device_name, fields.join(","));
                        let write_url = format!(
                            "{}/api/v2/write?org=-&bucket={}/{}",
                            influx.influx_url,
                            influx.influx_database,
                            influx.influx_retention_policy
                        );
                        let token = format!("{}:{}", influx.influx_user, influx.influx_pass);

                        let client = http_client.clone();
                        tokio::spawn(async move {
                            match client
                                .post(&write_url)
                                .header("Authorization", format!("Token {}", token))
                                .body(line)
                                .send()
                                .await
                            {
                                Ok(resp) => {
                                    if !resp.status().is_success() {
                                        let text = resp.text().await.unwrap_or_default();
                                        println!(
                                            "InfluxDB forward failed: {} - {}",
                                            text, write_url
                                        );
                                    }
                                }
                                Err(e) => {
                                    println!("InfluxDB request error: {}", e);
                                }
                            }
                        });
                    }
                }
            }
        }
    });

    // Main MQTT subscription polling loop
    loop {
        match eventloop.poll().await {
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
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
