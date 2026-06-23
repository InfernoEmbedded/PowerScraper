#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct SolaxWifiConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: u64,
    pub timeout: u64,
    pub inverters: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct SolaxModbusConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: u64,
    pub timeout: u64,
    #[serde(alias = "power_budget_avg_samples")]
    pub power_budget_avg_samples: Option<usize>,
    #[serde(alias = "installer_password")]
    pub installer_password: Option<u16>,
    pub inverters: Vec<String>,
    pub hostnames: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct SolaxXHybridModbusConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: u64,
    pub timeout: u64,
    #[serde(alias = "power_budget_avg_samples")]
    pub power_budget_avg_samples: Option<usize>,
    #[serde(alias = "installer_password")]
    pub installer_password: Option<u16>,
    pub inverters: Vec<String>,
    pub hostnames: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct SerialMeterConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: u64,
    pub timeout: u64,
    pub baud: u32,
    pub parity: String, // 'N', 'E', 'O'
    pub stopbits: u8,
    pub ports: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct MQTTPowerMeterDeviceConfig {
    pub broker: String,
    pub port: Option<u16>,
    pub topic_total: Option<String>,
    pub topic_phase1: Option<String>,
    pub topic_phase2: Option<String>,
    pub topic_phase3: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(alias = "poll_period")]
    pub poll_period: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct MQTTPowerMeterConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: Option<u64>,
    pub meters: Vec<String>,
    #[serde(flatten)]
    pub meter_devices: HashMap<String, MQTTPowerMeterDeviceConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct EmonCMSConfig {
    pub server: String,
    pub api_key: String,
    pub timeout: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct InfluxConfig {
    pub influx_url: String,
    pub influx_database: String,
    pub influx_measurement: String,
    pub influx_user: String,
    pub influx_pass: String,
    pub influx_retention_policy: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct MqttBrokerConfig {
    pub broker: String,
    pub port: Option<u16>,
    #[serde(alias = "base_topic")]
    pub base_topic: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(alias = "home_assistant_discovery")]
    pub home_assistant_discovery: Option<bool>,
    #[serde(alias = "home_assistant_prefix")]
    pub home_assistant_prefix: Option<String>,
}

impl MqttBrokerConfig {
    pub fn is_ha_discovery_enabled(&self) -> bool {
        self.home_assistant_discovery.unwrap_or(true)
    }

    pub fn ha_discovery_prefix(&self) -> String {
        self.home_assistant_prefix
            .clone()
            .unwrap_or_else(|| "homeassistant".to_string())
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct BatteryControlInverter {
    pub phase: usize,
    #[serde(default)]
    pub use_total_power: bool,
    #[serde(default)]
    pub single_phase_charge_limit: f64,
    #[serde(default)]
    pub single_phase_discharge_limit: f64,
    pub max_charge: f64,
    pub max_discharge: f64,
    #[serde(default)]
    pub grace_capacity: u8,
    #[serde(default)]
    pub grace_power_threshold: f64,
    #[serde(default)]
    pub grace_charge_power: f64,
    #[serde(default)]
    pub control_grid_power: bool,
    #[serde(default)]
    pub tickle_remote_control: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct BatteryControlPeriod {
    pub start: String,
    pub end: String,
    pub min_charge: u8,
    pub grid_charge: bool,
    pub force_discharge: Option<f64>,
    #[serde(default)]
    pub grace: bool,
    #[serde(default)]
    pub prefer_battery: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct SolaxBatteryControlConfig {
    pub source: Option<String>,
    #[serde(default)]
    pub linked_batteries: bool,
    pub timezone: Option<String>,
    #[serde(default)]
    pub inverter: HashMap<String, BatteryControlInverter>,
    #[serde(default)]
    pub period: HashMap<String, BatteryControlPeriod>,
    pub grid_target: Option<f64>,
    pub initial_mode: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Config {
    #[serde(rename = "Solax-Wifi")]
    pub solax_wifi: Option<SolaxWifiConfig>,

    #[serde(rename = "Solax-Modbus")]
    pub solax_modbus: Option<SolaxModbusConfig>,

    #[serde(rename = "Solax-XHybrid-Modbus")]
    pub solax_xhybrid_modbus: Option<SolaxXHybridModbusConfig>,

    #[serde(rename = "SDM630Modbusv2")]
    pub sdm630_modbus_v2: Option<SerialMeterConfig>,

    #[serde(rename = "DTSU666")]
    pub dtsu666: Option<SerialMeterConfig>,

    #[serde(rename = "MQTTPowerMeter")]
    pub mqtt_power_meter: Option<MQTTPowerMeterConfig>,

    #[serde(rename = "emoncms")]
    pub emoncms: Option<EmonCMSConfig>,

    #[serde(rename = "influx")]
    pub influx: Option<InfluxConfig>,

    #[serde(rename = "MQTT")]
    pub mqtt: Option<MqttBrokerConfig>,

    #[serde(rename = "Solax-BatteryControl")]
    pub battery_control: Option<SolaxBatteryControlConfig>,
}

impl Config {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn load_from_db(db_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                config_json TEXT NOT NULL
            )",
            [],
        )?;

        let mut stmt = conn.prepare("SELECT config_json FROM settings WHERE id = 1")?;
        let mut rows = stmt.query([])?;

        if let Some(row) = rows.next()? {
            let json_str: String = row.get(0)?;
            let config: Config = serde_json::from_str(&json_str)?;
            Ok(config)
        } else {
            // Seed database from config.toml if present, else config-sample.toml, else empty config
            let config = if Path::new("config.toml").exists() {
                println!("Seeding SQLite database from config.toml...");
                match Config::load_from_file("config.toml") {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        println!("Failed to load config.toml, using defaults: {}", e);
                        Config::default_empty()
                    }
                }
            } else if Path::new("config-sample.toml").exists() {
                println!("Seeding SQLite database from config-sample.toml...");
                match Config::load_from_file("config-sample.toml") {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        println!("Failed to load config-sample.toml, using defaults: {}", e);
                        Config::default_empty()
                    }
                }
            } else {
                Config::default_empty()
            };
            config.save_to_db(db_path)?;
            Ok(config)
        }
    }

    pub fn save_to_db(&self, db_path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                config_json TEXT NOT NULL
            )",
            [],
        )?;

        let json_str = serde_json::to_string_pretty(self)?;
        conn.execute(
            "INSERT OR REPLACE INTO settings (id, config_json) VALUES (1, ?1)",
            rusqlite::params![json_str],
        )?;
        Ok(())
    }

    pub fn default_empty() -> Self {
        Config {
            solax_wifi: None,
            solax_modbus: None,
            solax_xhybrid_modbus: None,
            sdm630_modbus_v2: None,
            dtsu666: None,
            mqtt_power_meter: None,
            emoncms: None,
            influx: None,
            mqtt: Some(MqttBrokerConfig {
                broker: "127.0.0.1".to_string(),
                port: Some(1883),
                base_topic: Some("sensors".to_string()),
                username: None,
                password: None,
                home_assistant_discovery: Some(true),
                home_assistant_prefix: Some("homeassistant".to_string()),
            }),
            battery_control: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_sample_file() {
        let sample_path = "config-sample.toml";
        let config_res = Config::load_from_file(sample_path);
        assert!(
            config_res.is_ok(),
            "Failed to parse config-sample.toml: {:?}",
            config_res.err()
        );
        let config = config_res.unwrap();
        assert!(config.solax_modbus.is_some());
        assert!(config.solax_xhybrid_modbus.is_some());
    }

    #[test]
    fn test_parse_custom_config() {
        let config_str = r#"
[MQTT]
broker = "127.0.0.1"
port = 1883
base_topic = "sensors"

[Solax-Wifi]
poll_period = 5
timeout = 5
inverters = ["192.168.1.10"]

[emoncms]
server = "http://localhost/emoncms"
api_key = "abcdef"
timeout = 10

[influx]
influx_url = "http://localhost:8086"
influx_database = "db"
influx_measurement = "m"
influx_user = "user"
influx_pass = "pass"
influx_retention_policy = "autogen"
        "#;
        let config: Config = toml::from_str(config_str).unwrap();
        assert!(config.mqtt.is_some());
        let mqtt = config.mqtt.as_ref().unwrap();
        assert!(mqtt.is_ha_discovery_enabled());
        assert_eq!(mqtt.ha_discovery_prefix(), "homeassistant");

        assert!(config.solax_wifi.is_some());
        assert!(config.emoncms.is_some());
        assert!(config.influx.is_some());

        // Test custom HA discovery config
        let custom_mqtt_str = r#"
            broker = "127.0.0.1"
            home-assistant-discovery = false
            home-assistant-prefix = "myha"
        "#;
        let custom_mqtt: MqttBrokerConfig = toml::from_str(custom_mqtt_str).unwrap();
        assert!(!custom_mqtt.is_ha_discovery_enabled());
        assert_eq!(custom_mqtt.ha_discovery_prefix(), "myha");
    }
}
