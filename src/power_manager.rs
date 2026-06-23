use crate::config::{
    BatteryControlInverter, BatteryControlPeriod, MqttBrokerConfig, SolaxBatteryControlConfig,
};
use crate::mqtt_helper::create_mqtt_client;
use chrono::{Local, NaiveTime, Timelike};
use rumqttc::{Event, Packet, QoS};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
struct InverterState {
    battery_capacity: u8,
    battery_power: f64,
    pv1_power: f64,
    pv2_power: f64,
    measured_power: f64,
    discharge_power: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerManagerMode {
    Auto,
    ChargeBatteries,
    MaximumFeedin,
}

impl std::fmt::Display for PowerManagerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PowerManagerMode::Auto => write!(f, "Auto"),
            PowerManagerMode::ChargeBatteries => write!(f, "ChargeBatteries"),
            PowerManagerMode::MaximumFeedin => write!(f, "MaximumFeedin"),
        }
    }
}

impl std::str::FromStr for PowerManagerMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let clean = s.trim().to_lowercase().replace(['_', ' '], "");
        match clean.as_str() {
            "auto" => Ok(PowerManagerMode::Auto),
            "chargebatteries" | "charge" => Ok(PowerManagerMode::ChargeBatteries),
            "maximumfeedin" | "maxfeedin" | "maximum" | "max" => {
                Ok(PowerManagerMode::MaximumFeedin)
            }
            _ => Err(()),
        }
    }
}

pub struct PowerManager {
    config: SolaxBatteryControlConfig,
    _base_topic: String,
    phase_power: [f64; 16],
    total_power: f64,
    assist_needed: HashMap<String, bool>,
    total_discharge_power: f64,
    max_total_charge_power: f64,
    max_total_discharge_power: f64,
    inverters: HashMap<String, InverterState>,
    config_inverters_count: usize,
    linked_batteries: bool,
    pub mode: PowerManagerMode,
    pub grid_target: f64,
}

impl PowerManager {
    pub fn new(config: SolaxBatteryControlConfig, base_topic: String) -> Self {
        let mut max_total_charge = 0.0;
        let mut max_total_discharge = 0.0;
        let mut assist_needed = HashMap::new();
        let mut inverters = HashMap::new();

        for (name, inv_cfg) in &config.inverter {
            max_total_charge += inv_cfg.max_charge;
            max_total_discharge += inv_cfg.max_discharge;
            assist_needed.insert(name.clone(), false);
            inverters.insert(name.clone(), InverterState::default());
        }

        let inverters_count = config.inverter.len();
        let linked_batteries = config.linked_batteries;

        let mode = config
            .initial_mode
            .as_ref()
            .and_then(|m| m.parse::<PowerManagerMode>().ok())
            .unwrap_or(PowerManagerMode::Auto);
        let grid_target = config.grid_target.unwrap_or(0.0);

        PowerManager {
            config,
            _base_topic: base_topic,
            phase_power: [0.0; 16],
            total_power: 0.0,
            assist_needed,
            total_discharge_power: 0.0,
            max_total_charge_power: max_total_charge,
            max_total_discharge_power: max_total_discharge,
            inverters,
            config_inverters_count: inverters_count,
            linked_batteries,
            mode,
            grid_target,
        }
    }

    fn get_period(&self) -> Option<&BatteryControlPeriod> {
        let now_time = Local::now().time();
        let now = NaiveTime::from_hms_opt(now_time.hour(), now_time.minute(), now_time.second())?;

        for period in self.config.period.values() {
            let start = parse_time(&period.start)?;
            let end = parse_time(&period.end)?;

            if start < end {
                if now >= start && now < end {
                    return Some(period);
                }
            } else {
                if !(now >= end && now < start) {
                    return Some(period);
                }
            }
        }
        None
    }

    #[allow(dead_code)]
    pub fn handle_meter_power(&mut self, total: f64, p1: f64, p2: f64, p3: f64) {
        self.total_power = total;
        self.phase_power[1] = p1;
        self.phase_power[2] = p2;
        self.phase_power[3] = p3;
    }

    pub fn handle_inverter_power(&mut self, name: &str, phase: usize, measured_power: f64) {
        self.phase_power[phase] = measured_power * -1.0;
        self.total_power = self.phase_power[1] + self.phase_power[2] + self.phase_power[3];
        if let Some(inv) = self.inverters.get_mut(name) {
            inv.measured_power = measured_power;
        }
    }

    pub fn evaluate_and_command(&mut self, inverter_name: &str) -> Option<i32> {
        let inverter_config = self.config.inverter.get(inverter_name)?.clone();
        let period = self.get_period()?.clone();

        // Ensure state entry exists
        if !self.inverters.contains_key(inverter_name) {
            self.inverters
                .insert(inverter_name.to_string(), InverterState::default());
        }

        let mut inv_state = self.inverters.get(inverter_name).unwrap().clone();
        let num_inverters = self.config_inverters_count as f64;

        let charge_val_opt = match self.mode {
            PowerManagerMode::ChargeBatteries => {
                inv_state.discharge_power = -inverter_config.max_charge;
                self.assist_needed.insert(inverter_name.to_string(), false);
                let charge_val = Self::discharge_at(
                    &inverter_config,
                    &period,
                    inv_state.discharge_power,
                    inv_state.battery_capacity,
                );
                Some(charge_val)
            }
            PowerManagerMode::MaximumFeedin => {
                inv_state.discharge_power = inverter_config.max_discharge;
                self.assist_needed.insert(inverter_name.to_string(), true);
                let charge_val = Self::discharge_at(
                    &inverter_config,
                    &period,
                    inv_state.discharge_power,
                    inv_state.battery_capacity,
                );
                Some(charge_val)
            }
            PowerManagerMode::Auto => {
                if period.grid_charge && inv_state.battery_capacity < period.min_charge {
                    inv_state.discharge_power = -inverter_config.max_charge;
                    self.assist_needed.insert(inverter_name.to_string(), false);
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                } else if inv_state.battery_capacity < period.min_charge && period.prefer_battery {
                    inv_state.discharge_power = -inv_state.pv1_power - inv_state.pv2_power;
                    if inv_state.discharge_power < -inverter_config.max_charge {
                        inv_state.discharge_power = -inverter_config.max_charge;
                    }
                    self.assist_needed.insert(inverter_name.to_string(), true);
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                } else {
                    // Try to zero power deviation from grid_target
                    let phase = inverter_config.phase;
                    let phase_power_val = self.phase_power[phase];
                    let error = if inverter_config.use_total_power {
                        self.total_power - self.grid_target
                    } else {
                        phase_power_val - (self.grid_target / num_inverters)
                    };
                    inv_state.discharge_power += error * 0.25;

                    // Update assist_needed
                    let assist_needed_val =
                        *self.assist_needed.get(inverter_name).unwrap_or(&false);
                    if assist_needed_val {
                        let lower_limit =
                            inverter_config.single_phase_discharge_limit / num_inverters;
                        let upper_limit =
                            -inverter_config.single_phase_charge_limit / num_inverters;
                        if (inv_state.discharge_power >= 0.0
                            && inv_state.discharge_power < lower_limit)
                            || (inv_state.discharge_power < 0.0
                                && inv_state.discharge_power > upper_limit)
                        {
                            self.assist_needed.insert(inverter_name.to_string(), false);
                        }
                    } else {
                        let val = inv_state.discharge_power + error * 0.75;
                        if val > inverter_config.single_phase_discharge_limit
                            || val < -inverter_config.single_phase_charge_limit
                        {
                            self.assist_needed.insert(inverter_name.to_string(), true);
                        }
                    }

                    // Battery Capacity low limit
                    if inv_state.battery_capacity <= period.min_charge
                        && inv_state.discharge_power > 0.0
                    {
                        inv_state.discharge_power = 0.0;
                        self.assist_needed.insert(inverter_name.to_string(), true);
                        let charge_val = Self::discharge_at(
                            &inverter_config,
                            &period,
                            inv_state.discharge_power,
                            inv_state.battery_capacity,
                        );
                        Some(charge_val)
                    } else {
                        // Assistance power load share
                        let any_assist = self.assist_needed.values().any(|&v| v);
                        if any_assist {
                            let total_error = self.total_power - self.grid_target;
                            if self.linked_batteries {
                                self.total_discharge_power += total_error * 0.1;
                                if self.total_discharge_power > self.max_total_discharge_power {
                                    self.total_discharge_power = self.max_total_discharge_power;
                                } else if self.total_discharge_power < -self.max_total_charge_power
                                {
                                    self.total_discharge_power = -self.max_total_charge_power;
                                }
                                inv_state.discharge_power =
                                    self.total_discharge_power / num_inverters;
                            } else {
                                let phase_error =
                                    phase_power_val - (self.grid_target / num_inverters);
                                inv_state.discharge_power -= phase_error * 0.25;
                                inv_state.discharge_power += total_error * 0.1;
                            }
                        }

                        // Clamp values
                        if inv_state.discharge_power > inverter_config.max_discharge {
                            inv_state.discharge_power = inverter_config.max_discharge;
                        } else if inv_state.discharge_power < -inverter_config.max_charge {
                            inv_state.discharge_power = -inverter_config.max_charge;
                        }

                        // BMS Throttling
                        if inv_state.battery_capacity > 95
                            && inv_state.discharge_power < 0.0
                            && inv_state.battery_power < (inv_state.discharge_power / -10.0)
                        {
                            inv_state.discharge_power = 0.0;
                            self.assist_needed.insert(inverter_name.to_string(), true);
                        }

                        // Grace period
                        let grace = period.grace
                            && inverter_config.grace_capacity > 0
                            && inverter_config.grace_charge_power > 0.0;
                        if grace
                            && inv_state.discharge_power < 0.0
                            && inv_state.battery_capacity > inverter_config.grace_capacity
                        {
                            let total_pv = inv_state.pv1_power + inv_state.pv2_power;
                            if total_pv < inverter_config.grace_power_threshold {
                                inv_state.discharge_power = 0.0;
                                self.assist_needed.insert(inverter_name.to_string(), true);
                            } else if inv_state.discharge_power
                                < -inverter_config.grace_charge_power
                            {
                                inv_state.discharge_power = -inverter_config.grace_charge_power;
                            }
                        }

                        let charge_val = Self::discharge_at(
                            &inverter_config,
                            &period,
                            inv_state.discharge_power,
                            inv_state.battery_capacity,
                        );
                        Some(charge_val)
                    }
                }
            }
        };

        self.inverters.insert(inverter_name.to_string(), inv_state);
        charge_val_opt
    }

    fn discharge_at(
        in_cfg: &BatteryControlInverter,
        period: &BatteryControlPeriod,
        mut power: f64,
        battery_capacity: u8,
    ) -> i32 {
        if let Some(fd) = period.force_discharge {
            power = fd;
        }

        if power > in_cfg.max_discharge {
            power = in_cfg.max_discharge;
        }

        if power < -in_cfg.max_charge {
            power = -in_cfg.max_charge;
        }

        if battery_capacity <= period.min_charge && power > 0.0 {
            power = 0.0;
        }

        -power as i32
    }
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(s, "%k:%M:%S"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%I:%M:%S %p"))
        .ok()
}

pub async fn run_power_manager_task(
    config: SolaxBatteryControlConfig,
    mqtt_config: MqttBrokerConfig,
    cancel_token: CancellationToken,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let client_id = "powerscraper-power-manager";
    let (mqtt_client, mut eventloop) = create_mqtt_client(client_id, &mqtt_config);

    let pm = Arc::new(Mutex::new(PowerManager::new(
        config.clone(),
        base_topic.clone(),
    )));

    // Subscribe to all meter and inverter status topics
    let status_wildcard = format!("{}/#", base_topic);
    if let Err(e) = mqtt_client
        .subscribe(&status_wildcard, QoS::AtLeastOnce)
        .await
    {
        println!("Power Manager failed to subscribe to status topic: {}", e);
        return;
    }

    println!(
        "Power Manager task running and subscribed to: {}",
        status_wildcard
    );

    // Publish Home Assistant discovery configs
    crate::mqtt_helper::publish_home_assistant_discovery(
        &mqtt_client,
        &mqtt_config,
        "power_manager",
        "mode",
        false,
    )
    .await;

    crate::mqtt_helper::publish_home_assistant_discovery(
        &mqtt_client,
        &mqtt_config,
        "power_manager",
        "grid_target",
        false,
    )
    .await;

    // Publish initial state and update global system status
    {
        let pm_lock = pm.lock().await;
        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
            status.active_mode = pm_lock.mode.to_string();
            status.grid_target = pm_lock.grid_target;
        }

        let mode_topic = format!("{}/power_manager/mode", base_topic);
        let _ = mqtt_client
            .publish(
                &mode_topic,
                QoS::AtLeastOnce,
                true,
                pm_lock.mode.to_string(),
            )
            .await;

        let target_topic = format!("{}/power_manager/grid_target", base_topic);
        let _ = mqtt_client
            .publish(
                &target_topic,
                QoS::AtLeastOnce,
                true,
                pm_lock.grid_target.to_string(),
            )
            .await;
    }

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            res = eventloop.poll() => {
                match res {
                    Ok(notification) => {
                        if let Event::Incoming(Packet::Publish(publish)) = notification {
                            // Extract topic components
                            // Expected structure: sensors/<device_name>/<metric>
                            let topic_suffix = publish.topic.strip_prefix(&format!("{}/", base_topic));
                            if let Some(suffix) = topic_suffix {
                                let parts: Vec<&str> = suffix.split('/').collect();
                                if parts.len() >= 2 {
                                    let device_name = parts[0];
                                    let metric = parts[1..].join("/");
                                    let payload = String::from_utf8_lossy(&publish.payload);
                                    let payload_trim = payload.trim();

                                    if device_name == "power_manager" {
                                        if metric == "command/mode" {
                                            if let Ok(new_mode) = payload_trim.parse::<PowerManagerMode>() {
                                                let mut pm_lock = pm.lock().await;
                                                pm_lock.mode = new_mode;
                                                println!("Power Manager mode changed to: {}", new_mode);

                                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                    status.active_mode = new_mode.to_string();
                                                }

                                                // Publish status update
                                                let status_topic =
                                                    format!("{}/power_manager/mode", base_topic);
                                                let _ = mqtt_client
                                                    .publish(
                                                        &status_topic,
                                                        QoS::AtLeastOnce,
                                                        true,
                                                        new_mode.to_string(),
                                                    )
                                                    .await;

                                                // Re-evaluate and command all inverters immediately
                                                let inverter_names: Vec<String> =
                                                    pm_lock.config.inverter.keys().cloned().collect();
                                                for inv_name in inverter_names {
                                                    if let Some(command_power) =
                                                        pm_lock.evaluate_and_command(&inv_name)
                                                    {
                                                        let cmd_topic = format!(
                                                            "{}/{}/command/charge_battery",
                                                            base_topic, inv_name
                                                        );
                                                        let _ = mqtt_client
                                                            .publish(
                                                                &cmd_topic,
                                                                QoS::AtLeastOnce,
                                                                false,
                                                                command_power.to_string(),
                                                            )
                                                            .await;
                                                    }
                                                }
                                            }
                                        } else if metric == "command/grid_target" {
                                            if let Ok(target) = payload_trim.parse::<f64>() {
                                                let mut pm_lock = pm.lock().await;
                                                pm_lock.grid_target = target;
                                                println!(
                                                    "Power Manager grid target changed to: {}W",
                                                    target
                                                );

                                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                    status.grid_target = target;
                                                }

                                                // Publish status update
                                                let status_topic =
                                                    format!("{}/power_manager/grid_target", base_topic);
                                                let _ = mqtt_client
                                                    .publish(
                                                        &status_topic,
                                                        QoS::AtLeastOnce,
                                                        true,
                                                        target.to_string(),
                                                    )
                                                    .await;

                                                // Re-evaluate and command all inverters immediately
                                                let inverter_names: Vec<String> =
                                                    pm_lock.config.inverter.keys().cloned().collect();
                                                for inv_name in inverter_names {
                                                    if let Some(command_power) =
                                                        pm_lock.evaluate_and_command(&inv_name)
                                                    {
                                                        let cmd_topic = format!(
                                                            "{}/{}/command/charge_battery",
                                                            base_topic, inv_name
                                                        );
                                                        let _ = mqtt_client
                                                            .publish(
                                                                &cmd_topic,
                                                                QoS::AtLeastOnce,
                                                                false,
                                                                command_power.to_string(),
                                                            )
                                                            .await;
                                                    }
                                                }
                                            }
                                        }
                                    } else if let Ok(val) = payload_trim.parse::<f64>() {
                                        let mut pm_lock = pm.lock().await;

                                        // 1. Check if it's the configured power consumption source
                                        let is_source = pm_lock
                                            .config
                                            .source
                                            .as_ref()
                                            .map(|s| s == device_name)
                                            .unwrap_or(false);
                                        if is_source {
                                            if metric == "Total system power" {
                                                pm_lock.total_power = val;
                                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                    status.meter_power = val;
                                                }
                                            } else if metric == "Phase 1 power" {
                                                pm_lock.phase_power[1] = val;
                                            } else if metric == "Phase 2 power" {
                                                pm_lock.phase_power[2] = val;
                                            } else if metric == "Phase 3 power" {
                                                pm_lock.phase_power[3] = val;
                                            }
                                        }

                                        // 2. Check if it's one of our configured participating inverters
                                        if pm_lock.config.inverter.contains_key(device_name) {
                                            // Update inverter-specific state
                                            let mut state = pm_lock
                                                .inverters
                                                .entry(device_name.to_string())
                                                .or_default()
                                                .clone();
                                            let mut updated = false;

                                            if metric == "Battery Capacity" {
                                                state.battery_capacity = val as u8;
                                                updated = true;
                                            } else if metric == "Battery Power" {
                                                state.battery_power = val;
                                                updated = true;
                                            } else if metric == "PV1 Power" {
                                                state.pv1_power = val;
                                                updated = true;
                                            } else if metric == "PV2 Power" {
                                                state.pv2_power = val;
                                                updated = true;
                                            } else if metric == "Measured Power" {
                                                state.measured_power = val;
                                                updated = true;
                                                // If source is not configured, we use inverter measured power
                                                if pm_lock.config.source.is_none()
                                                    && let Some(inv_cfg) =
                                                        pm_lock.config.inverter.get(device_name)
                                                {
                                                    let phase = inv_cfg.phase;
                                                    pm_lock.handle_inverter_power(device_name, phase, val);
                                                    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                        status.meter_power = pm_lock.total_power;
                                                    }
                                                }
                                            }

                                            if updated {
                                                pm_lock.inverters.insert(device_name.to_string(), state);

                                                // Recalculate control and command
                                                if let Some(command_power) =
                                                    pm_lock.evaluate_and_command(device_name)
                                                {
                                                    let cmd_topic = format!(
                                                        "{}/{}/command/charge_battery",
                                                        base_topic, device_name
                                                    );
                                                    let payload_str = command_power.to_string();
                                                    let _ = mqtt_client
                                                        .publish(
                                                            &cmd_topic,
                                                            QoS::AtLeastOnce,
                                                            false,
                                                            payload_str,
                                                        )
                                                        .await;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("Power Manager MQTT error: {}", e);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_period(
        start: &str,
        end: &str,
        min_charge: u8,
        grid_charge: bool,
        prefer_battery: bool,
    ) -> BatteryControlPeriod {
        BatteryControlPeriod {
            start: start.to_string(),
            end: end.to_string(),
            min_charge,
            grid_charge,
            force_discharge: None,
            grace: false,
            prefer_battery,
        }
    }

    fn mock_inverter(phase: usize, max_charge: f64, max_discharge: f64) -> BatteryControlInverter {
        BatteryControlInverter {
            phase,
            use_total_power: false,
            single_phase_charge_limit: 1000.0,
            single_phase_discharge_limit: 1000.0,
            max_charge,
            max_discharge,
            grace_capacity: 0,
            grace_power_threshold: 0.0,
            grace_charge_power: 0.0,
            control_grid_power: false,
            tickle_remote_control: false,
        }
    }

    #[test]
    fn test_parse_time() {
        assert_eq!(parse_time("06:55:00").unwrap().to_string(), "06:55:00");
        assert_eq!(parse_time("14:00:00").unwrap().to_string(), "14:00:00");
        assert_eq!(parse_time("22:05:00").unwrap().to_string(), "22:05:00");
        assert!(parse_time("invalid").is_none());
    }

    #[test]
    fn test_get_period() {
        let mut periods = HashMap::new();
        // Setup a period covering the whole day so it always matches
        periods.insert(
            "AllDay".to_string(),
            mock_period("00:00:00", "23:59:59", 20, false, false),
        );

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: HashMap::new(),
            period: periods,
            ..Default::default()
        };

        let pm = PowerManager::new(config, "sensors".to_string());
        let period = pm.get_period().unwrap();
        assert_eq!(period.min_charge, 20);
    }

    #[test]
    fn test_evaluate_and_command_grid_charge() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 30, true, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 2000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        // Battery capacity 15 < min_charge 30
        let state = InverterState {
            battery_capacity: 15,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        let cmd = pm.evaluate_and_command("solax1");
        // Should charge from grid at max rate.
        // inv_state.discharge_power = -max_charge = -2000.
        // discharge_at does: power * -1.0 = -2000 * -1.0 = 2000.
        assert_eq!(cmd, Some(2000));
    }

    #[test]
    fn test_evaluate_and_command_prefer_battery() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 35, false, true),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 2000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        let state = InverterState {
            battery_capacity: 25, // < 35
            pv1_power: 500.0,
            pv2_power: 300.0,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        let cmd = pm.evaluate_and_command("solax1");
        // discharge_power = 0 - pv1 - pv2 = -800.
        // discharge_at returns -800 * -1.0 = 800.
        assert_eq!(cmd, Some(800));
        assert!(*pm.assist_needed.get("solax1").unwrap());
    }

    #[test]
    fn test_evaluate_and_command_zero_phase_power() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 2000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        let state = InverterState {
            battery_capacity: 50, // Plenty of charge
            discharge_power: 100.0,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        // Mains phase 1 power is 400W.
        pm.phase_power[1] = 400.0;

        let cmd = pm.evaluate_and_command("solax1");
        // discharge_power = 100 + 400 * 0.25 = 200.
        // discharge_at returns 200 * -1 = -200.
        assert_eq!(cmd, Some(-200));
    }

    #[test]
    fn test_evaluate_and_command_bms_throttling() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 2000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        let state = InverterState {
            battery_capacity: 97,    // > 95%
            discharge_power: -500.0, // Wants to charge
            battery_power: -20.0,    // Battery power < discharge_power / -10 (which is 50)
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        let cmd = pm.evaluate_and_command("solax1");
        // Throttled by BMS -> discharge_power set to 0.
        // Returns 0.
        assert_eq!(cmd, Some(0));
    }

    #[test]
    fn test_evaluate_and_command_grace_period() {
        let mut periods = HashMap::new();
        let mut p = mock_period("00:00:00", "23:59:59", 10, false, false);
        p.grace = true;
        periods.insert("Always".to_string(), p);

        let mut inverters = HashMap::new();
        let mut inv = mock_inverter(1, 2000.0, 2000.0);
        inv.grace_capacity = 80;
        inv.grace_power_threshold = 3000.0;
        inv.grace_charge_power = 500.0;
        inverters.insert("solax1".to_string(), inv);

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        let state = InverterState {
            battery_capacity: 85,     // > grace_capacity 80
            discharge_power: -1000.0, // wants to charge at 1000W
            pv1_power: 1000.0,
            pv2_power: 500.0, // total solar 1500W < grace_power_threshold 3000W
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        let cmd = pm.evaluate_and_command("solax1");
        // Under grace capacity and low solar -> charge rate set to 0.
        assert_eq!(cmd, Some(0));
    }

    #[test]
    fn test_discharge_at_bounds() {
        let period = mock_period("00:00:00", "23:59:59", 20, false, false);
        let inverter = mock_inverter(1, 1500.0, 2000.0);

        // Power exceeds max_discharge
        let cmd1 = PowerManager::discharge_at(&inverter, &period, 2500.0, 50);
        assert_eq!(cmd1, -2000); // capped at max_discharge, negated

        // Power below max_charge
        let cmd2 = PowerManager::discharge_at(&inverter, &period, -3000.0, 50);
        assert_eq!(cmd2, 1500); // capped at max_charge, negated

        // Capacity <= min_charge, trying to discharge
        let cmd3 = PowerManager::discharge_at(&inverter, &period, 500.0, 15);
        assert_eq!(cmd3, 0); // clamp to 0
    }

    #[test]
    fn test_evaluate_and_command_linked_batteries() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 2000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: true,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        pm.total_power = 1000.0;

        let state = InverterState {
            battery_capacity: 50,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        pm.assist_needed.insert("solax1".to_string(), true);

        let cmd = pm.evaluate_and_command("solax1");
        assert!(cmd.is_some());
    }

    #[test]
    fn test_discharge_at_force_discharge() {
        let mut period = mock_period("00:00:00", "23:59:59", 20, false, false);
        period.force_discharge = Some(1200.0);
        let inverter = mock_inverter(1, 1500.0, 2000.0);

        let cmd = PowerManager::discharge_at(&inverter, &period, 500.0, 50);
        assert_eq!(cmd, -1200);
    }

    #[test]
    fn test_evaluate_and_command_use_total_power() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        let mut inv = mock_inverter(1, 2000.0, 2000.0);
        inv.use_total_power = true;
        inverters.insert("solax1".to_string(), inv);

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        pm.total_power = 800.0;

        let state = InverterState {
            battery_capacity: 50,
            discharge_power: 100.0,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-300));
    }

    #[test]
    fn test_get_period_midnight_wrap() {
        let now_time = Local::now().time();
        let hr = now_time.hour();
        let start_hr = (hr + 22) % 24;
        let end_hr = (hr + 20) % 24;

        let start_str = format!("{:02}:00:00", start_hr);
        let end_str = format!("{:02}:00:00", end_hr);

        let mut periods = HashMap::new();
        periods.insert(
            "WrapPeriod".to_string(),
            mock_period(&start_str, &end_str, 25, false, false),
        );

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: HashMap::new(),
            period: periods,
            ..Default::default()
        };

        let pm = PowerManager::new(config, "sensors".to_string());
        let period = pm.get_period();
        assert!(period.is_some());
        assert_eq!(period.unwrap().min_charge, 25);
    }

    #[test]
    fn test_power_manager_extra_coverage() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        let mut inv = mock_inverter(1, 2000.0, 2000.0);
        // Let's enable grace period settings to test line 232
        inv.grace_capacity = 80;
        inv.grace_power_threshold = 1000.0;
        inv.grace_charge_power = 300.0;
        inverters.insert("solax1".to_string(), inv);

        let config = SolaxBatteryControlConfig {
            source: Some("custom-meter".to_string()),
            linked_batteries: true,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        // 1. Call handle_meter_power to cover lines 90-95
        pm.handle_meter_power(500.0, 100.0, 200.0, 300.0);
        assert_eq!(pm.total_power, 500.0);
        assert_eq!(pm.phase_power[1], 100.0);
        assert_eq!(pm.phase_power[2], 200.0);
        assert_eq!(pm.phase_power[3], 300.0);

        // 2. Call handle_inverter_power to cover lines 97-103
        pm.handle_inverter_power("solax1", 1, 400.0);
        assert_eq!(pm.phase_power[1], -400.0); // measured_power * -1.0

        // 3. Test lines 110-113: evaluate_and_command for non-existent inverter to insert InverterState::default()
        // Remove it first to verify insertion behavior
        pm.inverters.remove("solax1");
        assert!(!pm.inverters.contains_key("solax1"));
        let _ = pm.evaluate_and_command("solax1");
        assert!(pm.inverters.contains_key("solax1"));

        // 4. Test lines 134-135: discharge_power < -max_charge
        // We set prefer_battery = true, min_charge = 60, battery_capacity = 50.
        // PV power is very high so discharge_power = -pv1 - pv2 is less than -max_charge.
        let mut periods_pref = HashMap::new();
        periods_pref.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 60, false, true),
        );
        pm.config.period = periods_pref;

        let state = InverterState {
            battery_capacity: 50,
            pv1_power: 1500.0,
            pv2_power: 1000.0, // sum = 2500 > max_charge (2000)
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(2000)); // capped at max_charge

        // 5. Test lines 160-161 / 163: assist_needed is true, but discharge power is within bounds -> assist_needed becomes false
        // We need to trigger Case 3 (Try to zero power) with assist_needed = true.
        // Let's use a period with prefer_battery = false and capacity = 50.
        let mut periods_normal = HashMap::new();
        periods_normal.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );
        pm.config.period = periods_normal;

        // assist_needed is true
        pm.assist_needed.insert("solax1".to_string(), true);
        pm.total_power = -100.0;
        pm.phase_power[1] = -100.0;

        // State has discharge_power = -50.0 (charging)
        // single_phase_charge_limit is 1000, so upper_limit is -1000.
        // discharge_power = -50.0 + phase_power_val * 0.25 = -50.0 + -100.0 * 0.25 = -75.0.
        // -75.0 is < 0.0 and > -1000.0, so it is within bounds -> assist_needed becomes false.
        let state_assist = InverterState {
            battery_capacity: 50,
            discharge_power: -50.0,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_assist);
        let _ = pm.evaluate_and_command("solax1");
        assert!(!*pm.assist_needed.get("solax1").unwrap());

        // 6. Test lines 175-177: capacity <= min_charge and discharge_power > 0.0 -> sets discharge_power to 0.0, assist_needed to true
        let state_low_cap = InverterState {
            battery_capacity: 5,    // <= min_charge 10
            discharge_power: 100.0, // > 0
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_low_cap);
        // phase power is positive, so it tries to discharge
        pm.total_power = 200.0;
        pm.phase_power[1] = 200.0;
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(0)); // discharge set to 0
        assert!(*pm.assist_needed.get("solax1").unwrap());

        // 7. Test lines 191-195: linked_batteries clamps total_discharge_power
        // We will call evaluate_and_command multiple times with high positive and negative total_power
        pm.config.linked_batteries = true;
        pm.linked_batteries = true;
        pm.max_total_discharge_power = 500.0;
        pm.max_total_charge_power = 500.0;
        pm.total_discharge_power = 450.0;

        let state_linked = InverterState {
            battery_capacity: 50,
            discharge_power: 1000.0, // Prevent assist_needed from clearing
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_linked);
        pm.assist_needed.insert("solax1".to_string(), true);

        // High positive power to trigger clamp to max_total_discharge_power
        pm.total_power = 1000.0;
        pm.phase_power[1] = 1000.0;
        let _ = pm.evaluate_and_command("solax1");
        assert_eq!(pm.total_discharge_power, 500.0);

        // High negative power to trigger clamp to -max_total_charge_power
        pm.total_power = -10000.0;
        pm.phase_power[1] = -10000.0; // Ensure negative phase power so assist_needed is not cleared
        let _ = pm.evaluate_and_command("solax1");
        assert_eq!(pm.total_discharge_power, -500.0);

        // 8. Test line 232: grace period clamp to -grace_charge_power
        // We need: grace = true (period.grace = true, grace_capacity > 0, grace_charge_power > 0)
        // discharge_power < 0
        // capacity > grace_capacity (capacity = 85, grace_capacity = 80)
        // total_pv >= grace_power_threshold (pv1 + pv2 = 1200 >= 1000)
        // discharge_power < -grace_charge_power (discharge_power = -500 < -300)
        let mut periods_grace = HashMap::new();
        let mut p_grace = mock_period("00:00:00", "23:59:59", 10, false, false);
        p_grace.grace = true;
        periods_grace.insert("Always".to_string(), p_grace);
        pm.config.period = periods_grace;
        pm.config.linked_batteries = false;
        pm.linked_batteries = false;

        let state_grace = InverterState {
            battery_capacity: 85,
            discharge_power: -500.0,
            pv1_power: 600.0,
            pv2_power: 600.0, // total 1200 >= 1000
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_grace);
        pm.total_power = 0.0;
        pm.phase_power[1] = 0.0;
        let cmd = pm.evaluate_and_command("solax1");
        // should be clamped to -grace_charge_power = -300 -> negated to 300
        assert_eq!(cmd, Some(300));
    }

    #[test]
    fn test_evaluate_and_command_modes() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        let inv = mock_inverter(1, 2000.0, 3000.0); // max charge 2000, max discharge 3000
        inverters.insert("solax1".to_string(), inv);

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            grid_target: Some(-100.0), // default grid target: feed in 100W
            initial_mode: Some("ChargeBatteries".to_string()),
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        assert_eq!(pm.mode, PowerManagerMode::ChargeBatteries);
        assert_eq!(pm.grid_target, -100.0);

        let state = InverterState {
            battery_capacity: 50,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state.clone());

        // 1. ChargeBatteries mode: should command max charge (2000)
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(2000));

        // 2. MaximumFeedin mode: should command max discharge (3000) -> negated is -3000
        pm.mode = PowerManagerMode::MaximumFeedin;
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-3000));

        // 3. Auto mode with grid_target of -100 (feed in 100W)
        pm.mode = PowerManagerMode::Auto;
        pm.phase_power[1] = -50.0;
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-3000));

        // Let's reset discharge_power to 0.0 to see target correction
        let state_zero = InverterState {
            battery_capacity: 50,
            discharge_power: 0.0,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_zero);
        pm.phase_power[1] = 400.0;
        // current grid import is 400W, we want -100W (feed in 100W).
        // error = 400 - (-100) = 500W.
        // discharge_power += 500 * 0.25 = 125W.
        // Returns -125.
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-125));

        // 4. Test PowerManagerMode parsing helper
        assert_eq!(
            "auto".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::Auto)
        );
        assert_eq!(
            "Charge_Batteries".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::ChargeBatteries)
        );
        assert_eq!(
            "MAX_FEEDIN".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::MaximumFeedin)
        );
        assert_eq!("invalid".parse::<PowerManagerMode>(), Err(()));
    }
}
