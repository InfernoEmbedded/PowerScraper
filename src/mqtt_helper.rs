use crate::config::MqttBrokerConfig;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

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

        let is_word_or_spaced = |target: &str| {
            m_lower == target
                || m_lower.starts_with(&format!("{} ", target))
                || m_lower.ends_with(&format!(" {}", target))
                || m_lower.contains(&format!(" {} ", target))
                || m_lower.starts_with(&format!("{}_", target))
                || m_lower.ends_with(&format!("_{}", target))
                || m_lower.contains(&format!("_{}_", target))
        };

        // 1. Power Factor (dimensionless ratio, no unit)
        if m_lower.contains("power factor")
            || m_lower.contains("power_factor")
            || is_word_or_spaced("pf")
        {
            val_payload["device_class"] = json!("power_factor");
            val_payload["state_class"] = json!("measurement");

        // 2. Frequency (Hz) - check before voltage to handle "Frequency of supply voltages"
        } else if m_lower.contains("frequency") || m_lower.contains("freq") || is_word_or_spaced("hz") {
            val_payload["unit_of_measurement"] = json!("Hz");
            val_payload["device_class"] = json!("frequency");
            val_payload["state_class"] = json!("measurement");

        // 3. Phase Angle (degrees)
        } else if m_lower.contains("phase angle")
            || m_lower.contains("phase_angle")
            || m_lower.contains("angle")
        {
            val_payload["unit_of_measurement"] = json!("°");
            val_payload["state_class"] = json!("measurement");

        // 4. THD (Total Harmonic Distortion - %) - check before voltage/current to handle "volts THD", "current THD"
        } else if m_lower.contains("thd") {
            val_payload["unit_of_measurement"] = json!("%");
            val_payload["state_class"] = json!("measurement");

        // 5. Reactive Energy (kvarh / varh) - check before reactive power & voltage
        } else if m_lower.contains("kvarh") || m_lower.contains("varh") {
            let unit = if m_lower.contains("kvarh") { "kvarh" } else { "varh" };
            val_payload["unit_of_measurement"] = json!(unit);
            val_payload["device_class"] = json!("reactive_energy");
            val_payload["state_class"] = json!("total_increasing");

        // 6. Apparent Energy (kVAh / VAh) - check before apparent power & voltage
        } else if m_lower.contains("kvah") || m_lower.contains("vah") {
            let unit = if m_lower.contains("kvah") { "kVAh" } else { "VAh" };
            val_payload["unit_of_measurement"] = json!(unit);
            val_payload["device_class"] = json!("apparent_energy");
            val_payload["state_class"] = json!("total_increasing");

        // 7. Reactive Power (var / kvar) - check before apparent power & voltage
        } else if m_lower.contains("volt amps reactive")
            || m_lower.contains("volt_amps_reactive")
            || m_lower.contains("reactive power")
            || m_lower.contains("reactive_power")
            || m_lower.contains("kvar")
            || is_word_or_spaced("var")
        {
            let unit = if m_lower.contains("kvar") { "kvar" } else { "var" };
            val_payload["unit_of_measurement"] = json!(unit);
            val_payload["device_class"] = json!("reactive_power");
            val_payload["state_class"] = json!("measurement");

        // 8. Apparent Power (VA / kVA) - check before voltage
        } else if m_lower.contains("volt amps")
            || m_lower.contains("volt_amps")
            || m_lower.contains("apparent")
            || m_lower.contains("kva")
            || is_word_or_spaced("va")
        {
            let unit = if m_lower.contains("kva") { "kVA" } else { "VA" };
            val_payload["unit_of_measurement"] = json!(unit);
            val_payload["device_class"] = json!("apparent_power");
            val_payload["state_class"] = json!("measurement");

        // 9. Battery / SOC / SOH (%)
        } else if m_lower.contains("capacity")
            || m_lower.contains("soc")
            || m_lower.contains("soh")
            || m_lower.contains("state of health")
        {
            val_payload["unit_of_measurement"] = json!("%");
            val_payload["device_class"] = json!("battery");
            val_payload["state_class"] = json!("measurement");

        // 10. Temperature (°C)
        } else if m_lower.contains("temp") {
            val_payload["unit_of_measurement"] = json!("°C");
            val_payload["device_class"] = json!("temperature");
            val_payload["state_class"] = json!("measurement");

        // 11. Electric Charge (Ah) - check before voltage/energy
        } else if is_word_or_spaced("ah")
            || is_word_or_spaced("mah")
            || m_lower.contains("amp hour")
            || m_lower.contains("ampere hour")
        {
            let unit = if is_word_or_spaced("mah") { "mAh" } else { "Ah" };
            val_payload["unit_of_measurement"] = json!(unit);
            val_payload["state_class"] = json!("total_increasing");

        // 12. Voltage / Volts (V)
        } else if m_lower.contains("voltage")
            || m_lower.contains("volts")
            || m_lower.contains("volt")
            || m_lower.contains("v_phase")
            || is_word_or_spaced("v")
        {
            val_payload["unit_of_measurement"] = json!("V");
            val_payload["device_class"] = json!("voltage");
            val_payload["state_class"] = json!("measurement");

        // 13. Current / Amps (A)
        } else if m_lower.contains("current")
            || m_lower.contains("amps")
            || m_lower.contains("amp")
            || m_lower.contains("a_phase")
            || is_word_or_spaced("a")
        {
            val_payload["unit_of_measurement"] = json!("A");
            val_payload["device_class"] = json!("current");
            val_payload["state_class"] = json!("measurement");

        // 14. Tariff / Electricity Price / Cost (c/kWh)
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

        // 15. Active Power (W) - MUST PRECEDE CUMULATIVE ENERGY!
        } else if m_lower.contains("power")
            || m_lower.contains("production")
            || m_lower.contains("consumption")
            || m_lower.contains("charging")
            || m_lower.contains("discharging")
            || m_lower.contains("budget")
            || m_lower.contains("usage")
            || m_lower.contains("demand")
            || is_word_or_spaced("w")
            || is_word_or_spaced("kw")
        {
            val_payload["unit_of_measurement"] = json!("W");
            val_payload["device_class"] = json!("power");
            val_payload["state_class"] = json!("measurement");

        // 16. Cumulative Energy (kWh)
        } else if m_lower.contains("energy")
            || m_lower.contains("kwh")
            || m_lower.contains("today")
            || m_lower.contains("yield")
            || m_lower.contains("solar total")
            || m_lower.contains("solar_total")
            || is_word_or_spaced("wh")
            || is_word_or_spaced("kwh")
            || is_word_or_spaced("mwh")
        {
            val_payload["unit_of_measurement"] = json!("kWh");
            val_payload["device_class"] = json!("energy");
            if m_lower.contains("stored") || m_lower.contains("available") {
                val_payload["state_class"] = json!("measurement");
            } else {
                val_payload["state_class"] = json!("total_increasing");
            }
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

#[derive(Debug, Clone, PartialEq)]
pub struct EnqueuedMqttMessage {
    pub topic: String,
    pub qos: QoS,
    pub retain: bool,
    pub payload: Vec<u8>,
}

/// Thread-safe deduplicating queue wrapper for MQTT publishes.
/// When enqueueing a message whose topic matches an existing queued topic,
/// the older message payload/qos/retain is replaced with the new parameters,
/// dropping the older message to prevent MQTT broker flooding.
#[derive(Clone)]
pub struct MqttEnqueueWrapper {
    inner: Arc<Mutex<MqttQueueState>>,
    notify: Arc<Notify>,
}

#[derive(Default)]
struct MqttQueueState {
    order: VecDeque<String>,
    messages: HashMap<String, EnqueuedMqttMessage>,
}

impl Default for MqttEnqueueWrapper {
    fn default() -> Self {
        Self::new()
    }
}

impl MqttEnqueueWrapper {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MqttQueueState::default())),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Enqueues an MQTT publish message.
    /// If `topic` is already present in the queue, the older queued message for that topic
    /// is dropped and replaced with the new payload and settings.
    pub fn enqueue<T, P>(&self, topic: T, qos: QoS, retain: bool, payload: P)
    where
        T: Into<String>,
        P: Into<Vec<u8>>,
    {
        let topic = topic.into();
        let payload = payload.into();
        let msg = EnqueuedMqttMessage {
            topic: topic.clone(),
            qos,
            retain,
            payload,
        };

        let mut state = self.inner.lock().unwrap();
        if state.messages.insert(topic.clone(), msg).is_none() {
            // Topic was not previously queued; add topic to FIFO ordering queue
            state.order.push_back(topic);
        }
        self.notify.notify_one();
    }

    /// Pops the next queued message for sending.
    pub fn pop(&self) -> Option<EnqueuedMqttMessage> {
        let mut state = self.inner.lock().unwrap();
        while let Some(topic) = state.order.pop_front() {
            if let Some(msg) = state.messages.remove(&topic) {
                return Some(msg);
            }
        }
        None
    }

    /// Asynchronously waits until a message is queued and pops it.
    pub async fn dequeue_async(&self) -> EnqueuedMqttMessage {
        loop {
            if let Some(msg) = self.pop() {
                return msg;
            }
            self.notify.notified().await;
        }
    }

    /// Returns the number of unique topics currently queued.
    pub fn len(&self) -> usize {
        let state = self.inner.lock().unwrap();
        state.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clears all pending messages from the queue.
    pub fn clear(&self) {
        let mut state = self.inner.lock().unwrap();
        state.order.clear();
        state.messages.clear();
    }

    /// Spawns a Tokio background task that continuously dequeues messages
    /// and publishes them via the provided `rumqttc::AsyncClient`.
    pub fn spawn_worker(&self, client: AsyncClient) -> tokio::task::JoinHandle<()> {
        let wrapper = self.clone();
        tokio::spawn(async move {
            loop {
                let msg = wrapper.dequeue_async().await;
                if let Err(e) = client.publish(&msg.topic, msg.qos, msg.retain, msg.payload).await {
                    eprintln!("MqttEnqueueWrapper worker publish error on {}: {}", msg.topic, e);
                }
            }
        })
    }
}

pub fn publish_mqtt_message(_topic: &str, _payload: &str) {
    // No-op or optional broadcast fallback if MQTT worker is inactive
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

        // Apparent Power (VA)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "EPS VA", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "VA");
        assert_eq!(payload["device_class"], "apparent_power");

        // MainsMeter Volt Amps (Must be VA, NOT V)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 volt amps", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "VA");
        assert_eq!(payload["device_class"], "apparent_power");

        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Total system volt amps", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "VA");
        assert_eq!(payload["device_class"], "apparent_power");

        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Total system VA demand", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "VA");
        assert_eq!(payload["device_class"], "apparent_power");

        // MainsMeter Reactive Power (Must be var, NOT V)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 volt amps reactive", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "var");
        assert_eq!(payload["device_class"], "reactive_power");

        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Total system VAr", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "var");
        assert_eq!(payload["device_class"], "reactive_power");

        // Frequency of supply voltages (Must be Hz, NOT V)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Frequency of supply voltages", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "Hz");
        assert_eq!(payload["device_class"], "frequency");

        // THD (Must be %, NOT V or A)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 L-N volts THD", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "%");

        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 current THD", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "%");

        // Reactive Energy (kvarh)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Total import kVArh", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "kvarh");
        assert_eq!(payload["device_class"], "reactive_energy");
        assert_eq!(payload["state_class"], "total_increasing");

        // Apparent Energy (VAh)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Total VAh", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "VAh");
        assert_eq!(payload["device_class"], "apparent_energy");
        assert_eq!(payload["state_class"], "total_increasing");

        // Ampere-hours (Ah)
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Ah", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "Ah");
        assert_eq!(payload["state_class"], "total_increasing");

        // Demand currents & power
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Phase 1 current demand", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "A");
        assert_eq!(payload["device_class"], "current");

        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "MainsMeter", "Total system power demand", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "W");
        assert_eq!(payload["device_class"], "power");

        // Solar Total energy
        let (_, _, payload) =
            build_discovery_payload(&mqtt_config, "inverter1", "Solar Total", false).unwrap();
        assert_eq!(payload["unit_of_measurement"], "kWh");
        assert_eq!(payload["device_class"], "energy");
        assert_eq!(payload["state_class"], "total_increasing");

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
        assert_eq!(p_kvar["unit_of_measurement"], "kvar");
        assert_eq!(p_kvar["device_class"], "reactive_power");

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

    #[tokio::test]
    async fn test_mqtt_enqueue_wrapper_deduplication() {
        let wrapper = MqttEnqueueWrapper::new();
        assert!(wrapper.is_empty());
        assert_eq!(wrapper.len(), 0);

        // Enqueue topic 1 twice with different payloads
        wrapper.enqueue("sensors/solax1/power", QoS::AtLeastOnce, false, "100.0");
        assert_eq!(wrapper.len(), 1);

        wrapper.enqueue("sensors/solax1/power", QoS::AtLeastOnce, false, "250.5");
        // Length should still be 1 (older topic payload dropped)
        assert_eq!(wrapper.len(), 1);

        // Enqueue a second topic
        wrapper.enqueue("sensors/solax1/voltage", QoS::AtLeastOnce, false, "230.1");
        assert_eq!(wrapper.len(), 2);

        // Dequeue first message -> should be updated payload "250.5"
        let msg1 = wrapper.pop().unwrap();
        assert_eq!(msg1.topic, "sensors/solax1/power");
        assert_eq!(msg1.payload, b"250.5");

        // Dequeue second message -> voltage
        let msg2 = wrapper.pop().unwrap();
        assert_eq!(msg2.topic, "sensors/solax1/voltage");
        assert_eq!(msg2.payload, b"230.1");

        assert!(wrapper.is_empty());
        assert!(wrapper.pop().is_none());
    }

    #[tokio::test]
    async fn test_mqtt_enqueue_wrapper_async_dequeue_and_clear() {
        let wrapper = MqttEnqueueWrapper::default();
        wrapper.enqueue("sensors/test", QoS::AtMostOnce, true, "hello");
        assert_eq!(wrapper.len(), 1);

        let wrapper_clone = wrapper.clone();
        let handle = tokio::spawn(async move {
            wrapper_clone.dequeue_async().await
        });

        let msg = handle.await.unwrap();
        assert_eq!(msg.topic, "sensors/test");
        assert_eq!(msg.payload, b"hello");

        wrapper.enqueue("sensors/test2", QoS::AtLeastOnce, false, "data");
        wrapper.clear();
        assert!(wrapper.is_empty());
    }
}
