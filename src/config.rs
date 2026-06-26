#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct SolaxWifiConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: f64,
    pub timeout: f64,
    pub inverters: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct SolaxModbusConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: f64,
    pub timeout: f64,
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
    pub poll_period: f64,
    pub timeout: f64,
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
    pub poll_period: f64,
    pub timeout: f64,
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
    #[serde(alias = "topic_total")]
    pub topic_total: Option<String>,
    #[serde(alias = "topic_phase1")]
    pub topic_phase1: Option<String>,
    #[serde(alias = "topic_phase2")]
    pub topic_phase2: Option<String>,
    #[serde(alias = "topic_phase3")]
    pub topic_phase3: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(alias = "poll_period")]
    pub poll_period: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct MQTTPowerMeterConfig {
    #[serde(alias = "poll_period")]
    pub poll_period: Option<f64>,
    pub meters: Vec<String>,
    #[serde(flatten)]
    pub meter_devices: HashMap<String, MQTTPowerMeterDeviceConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct MQTTInverterDeviceConfig {
    pub broker: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    
    #[serde(alias = "topic_pv1_power")]
    pub topic_pv1_power: Option<String>,
    #[serde(alias = "topic_pv2_power")]
    pub topic_pv2_power: Option<String>,
    #[serde(alias = "topic_pv1_voltage")]
    pub topic_pv1_voltage: Option<String>,
    #[serde(alias = "topic_pv2_voltage")]
    pub topic_pv2_voltage: Option<String>,
    #[serde(alias = "topic_pv1_current")]
    pub topic_pv1_current: Option<String>,
    #[serde(alias = "topic_pv2_current")]
    pub topic_pv2_current: Option<String>,
    #[serde(alias = "topic_grid_voltage")]
    pub topic_grid_voltage: Option<String>,
    #[serde(alias = "topic_grid_current")]
    pub topic_grid_current: Option<String>,
    #[serde(alias = "topic_grid_power")]
    pub topic_grid_power: Option<String>,
    #[serde(alias = "topic_frequency")]
    pub topic_frequency: Option<String>,
    #[serde(alias = "topic_temperature")]
    pub topic_temperature: Option<String>,
    #[serde(alias = "topic_energy_today")]
    pub topic_energy_today: Option<String>,
    #[serde(alias = "topic_energy_total")]
    pub topic_energy_total: Option<String>,
    #[serde(alias = "topic_battery_capacity")]
    pub topic_battery_capacity: Option<String>,
    #[serde(alias = "topic_battery_power")]
    pub topic_battery_power: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct MQTTInverterConfig {
    pub inverters: Vec<String>,
    #[serde(flatten)]
    pub inverter_devices: HashMap<String, MQTTInverterDeviceConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct EmonCMSConfig {
    pub server: String,
    pub api_key: String,
    pub timeout: f64,
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
    #[serde(default, alias = "use_total_power")]
    pub use_total_power: bool,
    #[serde(default, alias = "single_phase_charge_limit")]
    pub single_phase_charge_limit: f64,
    #[serde(default, alias = "single_phase_discharge_limit")]
    pub single_phase_discharge_limit: f64,
    #[serde(alias = "max_charge")]
    pub max_charge: f64,
    #[serde(alias = "max_discharge")]
    pub max_discharge: f64,
    #[serde(default, alias = "grace_capacity")]
    pub grace_capacity: u8,
    #[serde(default, alias = "grace_power_threshold")]
    pub grace_power_threshold: f64,
    #[serde(default, alias = "grace_charge_power")]
    pub grace_charge_power: f64,
    #[serde(default, alias = "control_grid_power")]
    pub control_grid_power: bool,
    #[serde(default, alias = "tickle_remote_control")]
    pub tickle_remote_control: bool,
    #[serde(default, alias = "battery_capacity")]
    pub battery_capacity: Option<f64>,
    #[serde(default, alias = "max_charge_pct")]
    pub max_charge_pct: Option<u8>,
    #[serde(default, alias = "min_charge_pct")]
    pub min_charge_pct: Option<u8>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct BatteryControlPeriod {
    pub start: String,
    pub end: String,
    #[serde(alias = "min_charge")]
    pub min_charge: u8,
    #[serde(alias = "grid_charge")]
    pub grid_charge: bool,
    #[serde(alias = "force_discharge")]
    pub force_discharge: Option<f64>,
    #[serde(default)]
    pub grace: bool,
    #[serde(default, alias = "prefer_battery")]
    pub prefer_battery: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct TouTariffPeriod {
    pub name: String,
    pub start: String,
    pub end: String,
    pub import_rate: f64,
    pub export_rate: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TariffConfig {
    #[serde(rename_all = "kebab-case")]
    Flat {
        import_rate: f64,
        export_rate: f64,
    },
    #[serde(rename_all = "kebab-case")]
    Tou {
        periods: Vec<TouTariffPeriod>,
    },
    #[serde(rename_all = "kebab-case")]
    Amber {
        api_key: String,
        site_id: String,
        #[serde(default)]
        negative_export_prevent: bool,
        #[serde(default)]
        low_price_charge: bool,
        #[serde(default)]
        low_price_threshold: f64,
        #[serde(default)]
        high_price_discharge: bool,
        #[serde(default)]
        high_price_threshold: f64,
        api_url: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct DemandConfig {
    pub start: String,
    pub end: String,
    pub rate: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct PvArrayConfig {
    pub name: String,
    pub capacity_w: f64,
    pub tilt: f64,
    pub azimuth: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct LocationConfig {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub arrays: Vec<PvArrayConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct SolaxBatteryControlConfig {
    pub source: Option<String>,
    #[serde(default, alias = "linked_batteries", alias = "linked-batteries")]
    pub linked_batteries: bool,
    pub timezone: Option<String>,
    #[serde(default, alias = "Inverter")]
    pub inverter: HashMap<String, BatteryControlInverter>,
    #[serde(default, alias = "Period")]
    pub period: HashMap<String, BatteryControlPeriod>,
    #[serde(alias = "grid_target")]
    pub grid_target: Option<f64>,
    #[serde(alias = "initial_mode")]
    pub initial_mode: Option<String>,
    pub tariff: Option<TariffConfig>,
    #[serde(default)]
    pub demand: Option<DemandConfig>,
}

fn default_flush_interval() -> u32 { 30 }
fn default_retention_days() -> Option<u32> { Some(365) }

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct HistoryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_flush_interval")]
    pub flush_interval_mins: u32,
    #[serde(default = "default_retention_days")]
    pub retention_days: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Config {
    #[serde(rename = "History")]
    pub history: Option<HistoryConfig>,

    #[serde(rename = "Solax-Wifi")]
    pub solax_wifi: Option<SolaxWifiConfig>,

    #[serde(rename = "Solax-Modbus")]
    pub solax_modbus: Option<SolaxModbusConfig>,

    #[serde(rename = "Solax-XHybrid-Modbus")]
    pub solax_xhybrid_modbus: Option<SolaxXHybridModbusConfig>,

    #[serde(rename = "SDM630Modbusv2", alias = "SDM630ModbusV2")]
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

    #[serde(rename = "MQTTInverter")]
    pub mqtt_inverter: Option<MQTTInverterConfig>,

    #[serde(rename = "Location")]
    pub location: Option<LocationConfig>,
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
            history: None,
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
            mqtt_inverter: None,
            location: None,
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

    #[test]
    fn test_load_python_config() {
        let config_str = r#"
[MQTT]
broker = "127.0.0.1"
port = 1883
base_topic = "sensors"

[Solax-Wifi]
poll_period = 5
timeout = 0.5
inverters = ["192.168.1.10"]

[SDM630ModbusV2]
poll_period = 1
timeout = 0.5
baud = 38400
parity = 'E'
stopbits = 1
ports = ["/dev/ttyUSB0"]

[MQTTPowerMeter]
poll_period = 10
meters = ["Meter1"]

[MQTTPowerMeter.Meter1]
broker = "mqtt.example.com"
port = 1883
topic_total = "sensors/TotalSystemPower"
topic_phase1 = "sensors/Phase1"
topic_phase2 = "sensors/Phase2"
topic_phase3 = "sensors/Phase3"
username = "mqtt_user"
password = "mqtt_password"
poll_period = 10

[Solax-BatteryControl]
source = "Meter1"
linked_batteries = true
timezone = "AEDT-10"

[Solax-BatteryControl.Inverter.solax1]
phase = 1
use_total_power = false
single_phase_charge_limit = 1000
single_phase_discharge_limit = 1000
max_charge = 2000
max_discharge = 2000
grace_capacity = 70
grace_power_threshold = 4500
grace_charge_power = 500
control_grid_power = true

[Solax-BatteryControl.Period.MorningPeak]
start = "6:55:00"
end = "9:05:00"
min_charge = 20
grid_charge = false
force_discharge = 2000
grace = false
prefer_battery = false
"#;
        let config_res = toml::from_str::<Config>(config_str);
        assert!(
            config_res.is_ok(),
            "Failed to parse Python-style configuration: {:?}",
            config_res.err()
        );
        let config = config_res.unwrap();
        
        assert!(config.mqtt.is_some());
        let mqtt = config.mqtt.as_ref().unwrap();
        assert_eq!(mqtt.base_topic, Some("sensors".to_string()));
        
        assert_eq!(config.solax_wifi.as_ref().unwrap().timeout, 0.5);
        
        assert!(config.sdm630_modbus_v2.is_some());
        let sdm = config.sdm630_modbus_v2.as_ref().unwrap();
        assert_eq!(sdm.baud, 38400);
        assert_eq!(sdm.ports[0], "/dev/ttyUSB0");
        assert_eq!(sdm.timeout, 0.5);
        
        assert!(config.mqtt_power_meter.is_some());
        let mqtt_meter = config.mqtt_power_meter.as_ref().unwrap();
        assert_eq!(mqtt_meter.meters[0], "Meter1");
        let meter1 = mqtt_meter.meter_devices.get("Meter1").unwrap();
        assert_eq!(meter1.topic_total, Some("sensors/TotalSystemPower".to_string()));
        assert_eq!(meter1.topic_phase1, Some("sensors/Phase1".to_string()));
        
        assert!(config.battery_control.is_some());
        let bat = config.battery_control.as_ref().unwrap();
        assert_eq!(bat.source, Some("Meter1".to_string()));
        assert!(bat.linked_batteries);
        assert_eq!(bat.timezone, Some("AEDT-10".to_string()));
        
        let inv1 = bat.inverter.get("solax1").unwrap();
        assert_eq!(inv1.phase, 1);
        assert!(!inv1.use_total_power);
        assert_eq!(inv1.single_phase_charge_limit, 1000.0);
        assert!(inv1.control_grid_power);
        
        let period1 = bat.period.get("MorningPeak").unwrap();
        assert_eq!(period1.start, "6:55:00");
        assert_eq!(period1.min_charge, 20);
        assert!(!period1.grid_charge);
    }

    #[test]
    fn test_parse_mqtt_inverter_config() {
        let config_str = r#"
[MQTTInverter]
inverters = ["aurora1"]

[MQTTInverter.aurora1]
broker = "homeautomation.lan"
username = "aurora10k"
password = "somepassword"
topic_pv1_power = "emon/aurora/power_in_1"
topic_pv2_power = "emon/aurora/power_in_2"
topic_pv1_voltage = "emon/aurora/v_in_1"
topic_pv2_voltage = "emon/aurora/v_in_2"
topic_pv1_current = "emon/aurora/i_in_1"
topic_pv2_current = "emon/aurora/i_in_2"
topic_grid_voltage = "emon/aurora/grid_voltage"
topic_grid_current = "emon/aurora/grid_current"
topic_grid_power = "emon/aurora/grid_power"
topic_frequency = "emon/aurora/frequency"
topic_temperature = "emon/aurora/temperature_inverter"
topic_energy_today = "emon/aurora/current_day"
topic_energy_total = "emon/aurora/total"
        "#;
        let config: Config = toml::from_str(config_str).unwrap();
        assert!(config.mqtt_inverter.is_some());
        let mqtt_inverter = config.mqtt_inverter.as_ref().unwrap();
        assert_eq!(mqtt_inverter.inverters[0], "aurora1");
        let aurora1 = mqtt_inverter.inverter_devices.get("aurora1").unwrap();
        assert_eq!(aurora1.broker, Some("homeautomation.lan".to_string()));
        assert_eq!(aurora1.username, Some("aurora10k".to_string()));
        assert_eq!(aurora1.password, Some("somepassword".to_string()));
        assert_eq!(aurora1.topic_pv1_power, Some("emon/aurora/power_in_1".to_string()));
        assert_eq!(aurora1.topic_pv2_power, Some("emon/aurora/power_in_2".to_string()));
        assert_eq!(aurora1.topic_pv1_voltage, Some("emon/aurora/v_in_1".to_string()));
        assert_eq!(aurora1.topic_pv2_voltage, Some("emon/aurora/v_in_2".to_string()));
        assert_eq!(aurora1.topic_pv1_current, Some("emon/aurora/i_in_1".to_string()));
        assert_eq!(aurora1.topic_pv2_current, Some("emon/aurora/i_in_2".to_string()));
        assert_eq!(aurora1.topic_grid_voltage, Some("emon/aurora/grid_voltage".to_string()));
        assert_eq!(aurora1.topic_grid_current, Some("emon/aurora/grid_current".to_string()));
        assert_eq!(aurora1.topic_grid_power, Some("emon/aurora/grid_power".to_string()));
        assert_eq!(aurora1.topic_frequency, Some("emon/aurora/frequency".to_string()));
        assert_eq!(aurora1.topic_temperature, Some("emon/aurora/temperature_inverter".to_string()));
        assert_eq!(aurora1.topic_energy_today, Some("emon/aurora/current_day".to_string()));
        assert_eq!(aurora1.topic_energy_total, Some("emon/aurora/total".to_string()));
    }
}
