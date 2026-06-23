use crate::config::{MqttBrokerConfig, SolaxWifiConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::QoS;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::time::{Duration, sleep};

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
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = format!("powerscraper-wifi-{}", inverter_host.replace('.', "-"));
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    // Spawn dummy MQTT loop to keep connection alive
    let host_mqtt = inverter_host.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = eventloop.poll().await {
                println!("Wifi Driver [{}] MQTT error: {}", host_mqtt, e);
                sleep(Duration::from_secs(5)).await;
            }
        }
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout))
        .build()
        .unwrap();

    let url = format!("http://{}/api/realTimeData.htm", inverter_host);
    let poll_interval = Duration::from_secs(config.poll_period);

    loop {
        match client.get(&url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.text().await {
                        Ok(raw_text) => {
                            let fixed_text = raw_text.replace(",,", ",0,").replace(",,", ",0,");
                            match serde_json::from_str::<WifiResponse>(&fixed_text) {
                                Ok(wifi_data) => {
                                    let vals = parse_wifi_data(&wifi_data, &inverter_host);

                                    for (metric, val) in vals {
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

        sleep(poll_interval).await;
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
    vals.insert("Battery Power".to_string(), get_val(15));
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
