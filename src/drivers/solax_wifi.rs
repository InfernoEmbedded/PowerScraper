use crate::config::{MqttBrokerConfig, SolaxWifiConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::QoS;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::time::{Duration, sleep};

use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
struct WifiResponse {
    #[serde(rename = "SN")]
    sn: String,
    #[serde(rename = "Data")]
    data: Vec<serde_json::Value>,
}

pub async fn run_solax_wifi_driver(
    inverter_host: String,
    config: SolaxWifiConfig,
    mqtt_config: MqttBrokerConfig,
    tx_telemetry: tokio::sync::mpsc::Sender<crate::dispatch_manager::TelemetryBatch>,
    cancel_token: CancellationToken,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = format!("powerscraper-wifi-{}", inverter_host.replace('.', "-"));
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    // Spawn dummy MQTT loop to keep connection alive
    let host_mqtt = inverter_host.clone();
    let cancel_token_clone = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => break,
                res = eventloop.poll() => {
                    if let Err(e) = res {
                        println!("Wifi Driver [{}] MQTT error: {}", host_mqtt, e);
                        tokio::select! {
                            _ = cancel_token_clone.cancelled() => break,
                            _ = sleep(Duration::from_secs(5)) => {}
                        }
                    }
                }
            }
        }
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(config.timeout))
        .build()
        .unwrap();

    let url = format!("http://{}/api/realTimeData.htm", inverter_host);
    let poll_interval = Duration::from_secs_f64(config.poll_period);

    let mut discovered_metrics = std::collections::HashSet::new();

    loop {
        if cancel_token.is_cancelled() {
            break;
        }

        match client.get(&url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.text().await {
                        Ok(raw_text) => {
                            let fixed_text = raw_text.replace(",,", ",0,").replace(",,", ",0,");
                            match serde_json::from_str::<WifiResponse>(&fixed_text) {
                                Ok(wifi_data) => {
                                    let vals = parse_wifi_data(&wifi_data, &inverter_host);

                                    // Extract status values for the web UI dashboard
                                    let bat_cap = vals
                                        .get("Battery Capacity")
                                        .and_then(|v| v.parse::<u8>().ok())
                                        .unwrap_or(0);
                                    let bat_pow = vals
                                        .get("Battery Power")
                                        .and_then(|v| v.parse::<i32>().ok())
                                        .unwrap_or(0);
                                    let pv1 = vals
                                        .get("PV1 Power")
                                        .and_then(|v| v.parse::<u32>().ok())
                                        .unwrap_or(0);
                                    let pv2 = vals
                                        .get("PV2 Power")
                                        .and_then(|v| v.parse::<u32>().ok())
                                        .unwrap_or(0);
                                    let run_mode = vals
                                        .get("Status")
                                        .and_then(|v| v.parse::<u32>().ok())
                                        .unwrap_or(0);

                                    if let Ok(mut status) =
                                        crate::web_server::get_system_status().lock()
                                    {
                                        let inv = status
                                            .inverters
                                            .entry(inverter_host.clone())
                                            .or_default();
                                        inv.battery_capacity = bat_cap;
                                        inv.battery_power = bat_pow;
                                        inv.pv_power = pv1 + pv2;
                                        inv.run_mode = run_mode;
                                        inv.last_updated = Some(std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs());
                                    }

                                    let mut numeric_metrics = std::collections::HashMap::new();
                                    for (metric, val_str) in &vals {
                                        if let Ok(num) = val_str.parse::<f64>() {
                                            numeric_metrics.insert(metric.clone(), num);
                                        }
                                    }
                                    if !numeric_metrics.is_empty() {
                                        let batch = crate::dispatch_manager::TelemetryBatch {
                                            device_name: inverter_host.clone(),
                                            timestamp: chrono::Utc::now().timestamp(),
                                            metrics: numeric_metrics,
                                        };
                                        let _ = tx_telemetry.try_send(batch);
                                    }

                                    for (metric, val) in vals {
                                        if !discovered_metrics.contains(&metric) {
                                            crate::mqtt_helper::publish_home_assistant_discovery(
                                                &mqtt_client,
                                                &mqtt_config,
                                                &inverter_host,
                                                &metric,
                                                false,
                                            )
                                            .await;
                                            discovered_metrics.insert(metric.clone());
                                        }

                                        let topic =
                                            format!("{}/{}/{}", base_topic, inverter_host, metric);
                                        let _ = mqtt_client
                                            .publish(&topic, QoS::AtMostOnce, false, val)
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    println!(
                                        "Wifi Driver [{}] failed to parse JSON: {}",
                                        inverter_host, e
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            println!(
                                "Wifi Driver [{}] failed to read response text: {}",
                                inverter_host, e
                            );
                        }
                    }
                } else {
                    println!(
                        "Wifi Driver [{}] returned non-success status code: {}",
                        inverter_host,
                        resp.status()
                    );
                }
            }
            Err(e) => {
                println!("Wifi Driver [{}] HTTP request failed: {}", inverter_host, e);
            }
        }

        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = sleep(poll_interval) => {}
        }
    }
}

fn parse_wifi_data(resp: &WifiResponse, host: &str) -> HashMap<String, String> {
    let mut vals = HashMap::new();
    vals.insert("name".to_string(), host.to_string());
    vals.insert("Serial".to_string(), resp.sn.clone());

    let get_val = |idx: usize| -> String {
        resp.data
            .get(idx)
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_else(|| "0".to_string())
    };

    vals.insert("PV1 Current".to_string(), get_val(0));
    vals.insert("PV2 Current".to_string(), get_val(1));
    vals.insert("PV1 Voltage".to_string(), get_val(2));
    vals.insert("PV2 Voltage".to_string(), get_val(3));
    vals.insert("Grid Current".to_string(), get_val(4));
    vals.insert("Grid Voltage".to_string(), get_val(5));
    vals.insert("Grid Power".to_string(), get_val(6));
    vals.insert("Inner Temp".to_string(), get_val(7));
    vals.insert("Solar Today".to_string(), get_val(8));
    vals.insert("Solar Total".to_string(), get_val(9));
    vals.insert("Feed In Power".to_string(), get_val(10));
    vals.insert("PV1 Power".to_string(), get_val(11));
    vals.insert("PV2 Power".to_string(), get_val(12));
    vals.insert("Battery Voltage".to_string(), get_val(13));
    vals.insert("Battery Current".to_string(), get_val(14));
    let bat_pow_str = get_val(15);
    let bat_pow_negated = if let Ok(val) = bat_pow_str.parse::<f64>() {
        (-val).to_string()
    } else {
        bat_pow_str
    };
    vals.insert("Battery Power".to_string(), bat_pow_negated);
    vals.insert("Battery Temp".to_string(), get_val(16));
    vals.insert("Battery Capacity".to_string(), get_val(17));
    vals.insert("Solar Total 2".to_string(), get_val(19));
    vals.insert("Energy to Grid".to_string(), get_val(41));
    vals.insert("Energy from Grid".to_string(), get_val(42));
    vals.insert("Grid Frequency".to_string(), get_val(50));
    vals.insert("EPS Voltage".to_string(), get_val(53));
    vals.insert("EPS Current".to_string(), get_val(54));
    vals.insert("EPS VA".to_string(), get_val(55));
    vals.insert("EPS Frequency".to_string(), get_val(56));
    vals.insert("Status".to_string(), get_val(67));

    vals
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_wifi_data() {
        let response_data = WifiResponse {
            sn: "TEST_WIFI_SN".to_string(),
            data: vec![
                json!("1.5"),    // PV1 Current
                json!("2.5"),    // PV2 Current
                json!("240.0"),  // PV1 Voltage
                json!("240.0"),  // PV2 Voltage
                json!("10.0"),   // Grid Current
                json!("230.0"),  // Grid Voltage
                json!("2300.0"), // Grid Power
                json!("45.0"),   // Inner Temp
                json!("12.5"),   // Solar Today
                json!("1250.0"), // Solar Total
                json!("5.0"),    // Feed In Power
                json!("360.0"),  // PV1 Power
                json!("600.0"),  // PV2 Power
                json!("54.0"),   // Battery Voltage
                json!("20.0"),   // Battery Current
                json!("1080.0"), // Battery Power
                json!("35.0"),   // Battery Temp
                json!("85"),     // Battery Capacity
            ],
        };

        let parsed = parse_wifi_data(&response_data, "127.0.0.1");
        assert_eq!(parsed.get("name").unwrap(), "127.0.0.1");
        assert_eq!(parsed.get("Serial").unwrap(), "TEST_WIFI_SN");
        assert_eq!(parsed.get("PV1 Current").unwrap(), "1.5");
        assert_eq!(parsed.get("Battery Capacity").unwrap(), "85");
    }
}
