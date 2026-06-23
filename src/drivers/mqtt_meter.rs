use crate::config::{MQTTPowerMeterDeviceConfig, MqttBrokerConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::{Event, Packet, QoS};
use std::time::Duration;
use tokio::time::sleep;

pub async fn run_mqtt_meter_driver(
    meter_name: String,
    config: MQTTPowerMeterDeviceConfig,
    mqtt_config: MqttBrokerConfig,
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

    loop {
        match eventloop.poll().await {
            Ok(notification) => {
                if let Event::Incoming(Packet::Publish(publish)) = notification {
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
            }
            Err(e) => {
                println!("MQTT Meter [{}] connection error: {}", meter_name, e);
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
