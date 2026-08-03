use crate::config::MqttBrokerConfig;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
use serde_json::json;
use std::time::Duration;

pub fn create_mqtt_client(client_id: &str, config: &MqttBrokerConfig) -> (AsyncClient, EventLoop) {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string());
    let pid = std::process::id();
    let unique_client_id = if client_id.is_empty() {
        format!("PowerScraper-{}-{}", hostname, pid)
    } else {
        format!("PowerScraper-{}-{}-{}", hostname, pid, client_id)
    };

    let port = config.port.unwrap_or(1883);
    let mut options = MqttOptions::new(unique_client_id, &config.broker, port);
    options.set_keep_alive(Duration::from_secs(60));
    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        if !username.is_empty() {
            options.set_credentials(username, password);
        }
    }
    AsyncClient::new(options, 10000)
}

pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .replace("__", "_")
        .trim_matches('_')
        .to_string()
}

pub fn build_discovery_payload(
    mqtt_config: &MqttBrokerConfig,
    device_name: &str,
    metric_name: &str,
    is_command: bool,
) -> Option<(String, String, serde_json::Value)> {
    if !mqtt_config.is_ha_discovery_enabled() {
        return None;
    }

    let discovery_prefix = mqtt_config.ha_discovery_prefix();
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());

    let device_sanitized = sanitize_id(device_name);
    let metric_sanitized = sanitize_id(metric_name);
    let unique_id = format!("powerscraper_{}_{}", device_sanitized, metric_sanitized);

    // Common device block for UI grouping
    let device_info = if device_name == "power_manager" {
        json!({
            "identifiers": ["powerscraper_power_manager"],
            "name": "PowerScraper Power Manager",
            "model": "PowerScraper Power Manager",
            "manufacturer": "PowerScraper"
        })
    } else if device_name == "aggregate" {
        json!({
            "identifiers": ["powerscraper_aggregate"],
            "name": "PowerScraper Aggregate Sensors",
            "model": "PowerScraper Aggregate Sensors",
            "manufacturer": "PowerScraper"
        })
    } else {
        json!({
            "identifiers": [format!("powerscraper_{}", device_sanitized)],
            "name": device_name,
            "model": "PowerScraper Device",
            "manufacturer": "PowerScraper"
        })
    };

    let (component, discovery_topic, payload) = if device_name == "power_manager" {
        if metric_name == "mode" {
            let config_topic = format!(
                "{}/select/{}/mode/config",
                discovery_prefix, device_sanitized
            );
            let state_topic = format!("{}/power_manager/mode", base_topic);
            let command_topic = format!("{}/power_manager/command/mode", base_topic);
            let payload = json!({
                "name": "Mode",
                "state_topic": state_topic,
                "command_topic": command_topic,
                "unique_id": unique_id,
                "device": device_info,
                "options": ["Auto", "ChargeBatteries", "MaximumFeedin"]
            });
            ("select", config_topic, payload)
        } else if metric_name == "grid_target" {
            let config_topic = format!(
                "{}/number/{}/grid_target/config",
                discovery_prefix, device_sanitized
            );
            let state_topic = format!("{}/power_manager/grid_target", base_topic);
            let command_topic = format!("{}/power_manager/command/grid_target", base_topic);
            let payload = json!({
                "name": "Grid Target",
                "state_topic": state_topic,
                "command_topic": command_topic,
                "unique_id": unique_id,
                "device": device_info,
                "min": -10000,
                "max": 10000,
                "step": 1,
                "unit_of_measurement": "W",
                "device_class": "power"
            });
            ("number", config_topic, payload)
        } else {
            return None;
        }
    } else if is_command {
        // Assume inverter battery charge command
        let config_topic = format!(
            "{}/number/{}/{}/config",
            discovery_prefix, device_sanitized, metric_sanitized
        );
        let state_topic = format!("{}/{}/Requested Battery Power", base_topic, device_name);
        let command_topic = format!("{}/{}/command/charge_battery", base_topic, device_name);
        let payload = json!({
            "name": format!("{} Charge Battery", device_name),
            "state_topic": state_topic,
            "command_topic": command_topic,
            "unique_id": unique_id,
            "device": device_info,
            "min": -10000,
            "max": 10000,
            "step": 1,
            "unit_of_measurement": "W",
            "device_class": "power"
        });
        ("number", config_topic, payload)
    } else {
        // Sensor entity
        let config_topic = format!(
            "{}/sensor/{}/{}/config",
            discovery_prefix, device_sanitized, metric_sanitized
        );
        let state_topic = format!("{}/{}/{}", base_topic, device_name, metric_name);

        let mut val_payload = json!({
            "name": metric_name,
            "state_topic": state_topic,
            "unique_id": unique_id,
            "device": device_info
        });

        // Deduce device class, unit, and state class
        let m_lower = metric_name.to_lowercase();

        // 1. Power Factor (dimensionless ratio, no unit)
        if m_lower.contains("power factor")
            || m_lower.contains("power_factor")
            || m_lower == "pf"
            || m_lower.ends_with(" pf")
        {
            val_payload["device_class"] = json!("power_factor");
            val_payload["state_class"] = json!("measurement");

        // 2. Phase Angle (degrees)
        } else if m_lower.contains("phase angle")
            || m_lower.contains("phase_angle")
            || m_lower.contains("angle")
        {
            val_payload["unit_of_measurement"] = json!("°");
            val_payload["state_class"] = json!("measurement");

        // 3. THD (Total Harmonic Distortion - %)
        } else if m_lower.contains("thd") {
            val_payload["unit_of_measurement"] = json!("%");
            val_payload["state_class"] = json!("measurement");

        // 4. Voltage / Volts (V)
        } else if m_lower.contains("voltage")
            || m_lower.contains("volts")
            || m_lower.contains("volt")
            || m_lower.contains("v_phase")
        {
            val_payload["unit_of_measurement"] = json!("V");
            val_payload["device_class"] = json!("voltage");
            val_payload["state_class"] = json!("measurement");

        // 5. Current / Amps (A)
        } else if m_lower.contains("current")
            || m_lower.contains("amps")
            || m_lower.contains("amp")
            || m_lower.contains("a_phase")
        {
            val_payload["unit_of_measurement"] = json!("A");
            val_payload["device_class"] = json!("current");
            val_payload["state_class"] = json!("measurement");

        // 6. Frequency (Hz)
        } else if m_lower.contains("frequency") || m_lower.contains("freq") {
            val_payload["unit_of_measurement"] = json!("Hz");
            val_payload["device_class"] = json!("frequency");
            val_payload["state_class"] = json!("measurement");

        // 7. Temperature (°C)
        } else if m_lower.contains("temp") {
            val_payload["unit_of_measurement"] = json!("°C");
            val_payload["device_class"] = json!("temperature");
            val_payload["state_class"] = json!("measurement");

        // 8. Battery / SOC / SOH (%)
        } else if m_lower.contains("capacity")
            || m_lower.contains("soc")
            || m_lower.contains("soh")
            || m_lower.contains("state of health")
        {
            val_payload["unit_of_measurement"] = json!("%");
            val_payload["device_class"] = json!("battery");
            val_payload["state_class"] = json!("measurement");

        // 9. Reactive Energy (kvarh)
        } else if m_lower.contains("kvarh") {
            val_payload["unit_of_measurement"] = json!("kvarh");
            val_payload["state_class"] = json!("total_increasing");

        // 10. Apparent Energy (kVAh)
        } else if m_lower.contains("kvah") {
            val_payload["unit_of_measurement"] = json!("kVAh");
            val_payload["state_class"] = json!("total_increasing");

        // 11. Reactive Power (var / kVAR)
        } else if m_lower.contains("kvar") || m_lower.contains("volt amps reactive") || m_lower.contains("reactive") {
            val_payload["unit_of_measurement"] = json!("var");
            val_payload["device_class"] = json!("reactive_power");
            val_payload["state_class"] = json!("measurement");

        // 12. Apparent Power (VA)
        } else if m_lower.contains("volt amps") || m_lower.contains("apparent") || m_lower.contains("va") {
            val_payload["unit_of_measurement"] = json!("VA");
            val_payload["device_class"] = json!("apparent_power");
            val_payload["state_class"] = json!("measurement");

        // 13. Tariff / Electricity Price / Cost (c/kWh)
        } else if m_lower.contains("price")
            || m_lower.contains("tariff")
            || m_lower.contains("rate")
            || m_lower.contains("cost")
            || m_lower.contains("feedin")
            || m_lower.contains("general")
        {
            val_payload["unit_of_measurement"] = json!("c/kWh");
            val_payload["device_class"] = json!("monetary");
            val_payload["state_class"] = json!("measurement");

        // 14. Active Power (W) - MUST PRECEDE CUMULATIVE ENERGY!
        } else if m_lower.contains("power")
            || m_lower.contains("production")
            || m_lower.contains("consumption")
            || m_lower.contains("charging")
            || m_lower.contains("discharging")
            || m_lower.contains("budget")
            || m_lower.contains("usage")
            || m_lower.contains("demand")
            || m_lower.ends_with(" w")
        {
            val_payload["unit_of_measurement"] = json!("W");
            val_payload["device_class"] = json!("power");
            val_payload["state_class"] = json!("measurement");

        // 14. Cumulative Energy (kWh)
        } else if m_lower.contains("energy")
            || m_lower.contains("kwh")
            || m_lower.contains("today")
            || m_lower.contains("yield")
            || m_lower.ends_with("h")
        {
            val_payload["unit_of_measurement"] = json!("kWh");
            val_payload["device_class"] = json!("energy");
            val_payload["state_class"] = json!("total_increasing");
        }

        ("sensor", config_topic, val_payload)
    };

    Some((component.to_string(), discovery_topic, payload))
}

pub async fn publish_home_assistant_discovery(
    client: &AsyncClient,
    mqtt_config: &MqttBrokerConfig,
    device_name: &str,
    metric_name: &str,
    is_command: bool,
) {
    if let Some((_component, discovery_topic, payload)) =
        build_discovery_payload(mqtt_config, device_name, metric_name, is_command)
    {
        if let Ok(payload_str) = serde_json::to_string(&payload) {
            if let Err(e) = client
                .publish(&discovery_topic, QoS::AtLeastOnce, true, payload_str)
                .await
            {
                println!(
                    "Failed to publish Home Assistant discovery for {} ({}): {}",
                    device_name, discovery_topic, e
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("192.168.1.10"), "192_168_1_10");
        assert_eq!(sanitize_id("PV1 Current"), "pv1_current");
        assert_eq!(
            sanitize_id("Total System Power (W)"),
            "total_system_power_w"
        );
        assert_eq!(sanitize_id("EPS VA (X1)"), "eps_va_x1");
    }

    #[test]
    fn test_build_discovery_payload_sensor_deduction() {
        let mqtt_config = MqttBrokerConfig {
            enabled: Some(true),
            broker: "localhost".to_string(),
            port: None,
            base_topic: Some("sensors".to_string()),
            username: None,
            password: None,
            home_assistant_discovery: Some(true),
            home_assistant_prefix: Some("ha".to_string()),
        };

        // Voltage
        let (_, topic, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "PV1 Voltage", false).unwrap();
        assert_eq!(topic, "ha/sensor/inverter1/pv1_voltage/config");
        assert_eq!(payload["unit_of_measurement"], "V");
        assert_eq!(payload["device_class"], "voltage");
        assert_eq!(payload["state_class"], "measurement");

        // Temperature
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "Inner Temp", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "°C");
        assert_eq!(payload["device_class"], "temperature");

        // Battery
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "Battery Capacity", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "%");
        assert_eq!(payload["device_class"], "battery");

        // Energy (Today)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "Energy Today", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "kWh");
        assert_eq!(payload["device_class"], "energy");
        assert_eq!(payload["state_class"], "total_increasing");

        // Apparent Power
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "EPS VA", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "VA");
        assert_eq!(payload["device_class"], "apparent_power");

        // MainsMeter Total Active Power (Must be W, not kWh)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Total active power", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "W");
        assert_eq!(payload["device_class"], "power");
        assert_eq!(payload["state_class"], "measurement");

        // MainsMeter Volts (Must be V)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 line to neutral volts", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "V");
        assert_eq!(payload["device_class"], "voltage");

        // MainsMeter Power Factor (Must be power_factor, no kWh/W unit)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 power factor", false).unwrap();
        assert_eq!(payload["device_class"], "power_factor");
        assert!(payload.get("unit_of_measurement").is_none());

        // MainsMeter Phase Angle (Must be °)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 phase angle", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "°");

        // Aggregate metric (Total Solar Production)
        let (_, topic, payload) =
            build_discovery_payload(&mqtt_config, "aggregate", "Total Solar Production", false)
                .unwrap();
        assert_eq!(topic, "ha/sensor/aggregate/total_solar_production/config");
        assert_eq!(payload["unit_of_measurement"], "W");
        assert_eq!(payload["device_class"], "power");
        assert_eq!(payload["state_class"], "measurement");
        assert_eq!(payload["device"]["name"], "PowerScraper Aggregate Sensors");
    }

    #[test]
    fn test_build_discovery_payload_commands() {
        let mqtt_config = MqttBrokerConfig {
            enabled: Some(true),
            broker: "localhost".to_string(),
            port: None,
            base_topic: Some("sensors".to_string()),
            username: None,
            password: None,
            home_assistant_discovery: Some(true),
            home_assistant_prefix: Some("ha".to_string()),
        };

        // Inverter charge command
        let (comp, topic, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "charge_battery", true).unwrap();
        assert_eq!(comp, "number");
        assert_eq!(topic, "ha/number/inverter1/charge_battery/config");
        assert_eq!(
            payload["command_topic"],
            "sensors/inverter1/command/charge_battery"
        );
        assert_eq!(
            payload["state_topic"],
            "sensors/inverter1/Requested Battery Power"
        );
        assert_eq!(payload["min"], -10000);
        assert_eq!(payload["max"], 10000);

        // Power manager mode
        let (comp, topic, payload) =
            build_discovery_payload(&mqtt_config, "power_manager", "mode", false).unwrap();
        assert_eq!(comp, "select");
        assert_eq!(topic, "ha/select/power_manager/mode/config");
        assert_eq!(
            payload["options"],
            json!(["Auto", "ChargeBatteries", "MaximumFeedin"])
        );

        // Power manager grid target
        let (comp, topic, payload) =
            build_discovery_payload(&mqtt_config, "power_manager", "grid_target", false).unwrap();
        assert_eq!(comp, "number");
        assert_eq!(topic, "ha/number/power_manager/grid_target/config");
        assert_eq!(payload["min"], -10000);
        assert_eq!(payload["max"], 10000);
    }

    #[test]
    fn test_mqtt_helper_extra_coverage() {
        // Test disabled discovery
        let disabled_config = MqttBrokerConfig {
            enabled: Some(true),
            broker: "localhost".to_string(),
            port: None,
            base_topic: None,
            username: None,
            password: None,
            home_assistant_discovery: Some(false),
            home_assistant_prefix: None,
        };
        assert!(build_discovery_payload(&disabled_config, "inverter", "PV1", false).is_none());

        // Test power manager invalid metric
        let enabled_config = MqttBrokerConfig {
            enabled: Some(true),
            broker: "localhost".to_string(),
            port: None,
            base_topic: None,
            username: None,
            password: None,
            home_assistant_discovery: Some(true),
            home_assistant_prefix: None,
        };
        assert!(build_discovery_payload(&enabled_config, "power_manager", "invalid_metric", false).is_none());

        // Test additional metric type deductions (kvarh, kvar, fallback)
        let (_, _, p_kvarh) = build_discovery_payload(&enabled_config, "inverter", "Total kvarh", false).unwrap();
        assert_eq!(p_kvarh["unit_of_measurement"], "kvarh");

        let (_, _, p_kvar) = build_discovery_payload(&enabled_config, "inverter", "System kvar", false).unwrap();
        assert_eq!(p_kvar["unit_of_measurement"], "var");

        let (_, _, p_fallback) = build_discovery_payload(&enabled_config, "inverter", "UnknownMetric", false).unwrap();
        assert!(p_fallback.get("unit_of_measurement").is_none());

        // Test create_mqtt_client function
        let config = MqttBrokerConfig {
            enabled: Some(true),
            broker: "127.0.0.1".to_string(),
            port: Some(1883),
            base_topic: Some("sensors".to_string()),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            home_assistant_discovery: Some(true),
            home_assistant_prefix: Some("ha".to_string()),
        };
        let (client, _eventloop) = create_mqtt_client("test_id", &config);
        // Clean cleanup - we don't start the loop, just verify instantiation
        drop(client);
    }
}
