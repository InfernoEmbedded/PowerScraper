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

#[derive(Debug, Clone, Default)]
struct InverterState {
    battery_capacity: u8,
    battery_power: f64,
    pv1_power: f64,
    pv2_power: f64,
    measured_power: f64,
    discharge_power: f64,
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

        // 1. Grid charge
        let charge_val_opt = if period.grid_charge && inv_state.battery_capacity < period.min_charge
        {
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
        // 2. Prefer battery
        else if inv_state.battery_capacity < period.min_charge && period.prefer_battery {
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
            // 3. Try to zero power
            let phase = inverter_config.phase;
            let phase_power_val = self.phase_power[phase];
            if inverter_config.use_total_power {
                inv_state.discharge_power += self.total_power * 0.25;
            } else {
                inv_state.discharge_power += phase_power_val * 0.25;
            }

            // 4. Update assist_needed
            let assist_needed_val = *self.assist_needed.get(inverter_name).unwrap_or(&false);
            let num_inverters = self.config_inverters_count as f64;
            if assist_needed_val {
                let lower_limit = inverter_config.single_phase_discharge_limit / num_inverters;
                let upper_limit = -inverter_config.single_phase_charge_limit / num_inverters;
                if (inv_state.discharge_power >= 0.0 && inv_state.discharge_power < lower_limit)
                    || (inv_state.discharge_power < 0.0 && inv_state.discharge_power > upper_limit)
                {
                    self.assist_needed.insert(inverter_name.to_string(), false);
                }
            } else {
                let val = inv_state.discharge_power + phase_power_val * 0.75;
                if val > inverter_config.single_phase_discharge_limit
                    || val < -inverter_config.single_phase_charge_limit
                {
                    self.assist_needed.insert(inverter_name.to_string(), true);
                }
            }

            // 5. Battery Capacity low limit
            if inv_state.battery_capacity <= period.min_charge && inv_state.discharge_power > 0.0 {
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
                // 6. Assistance power load share
                let any_assist = self.assist_needed.values().any(|&v| v);
                if any_assist {
                    if self.linked_batteries {
                        self.total_discharge_power += self.total_power * 0.1;
                        if self.total_discharge_power > self.max_total_discharge_power {
                            self.total_discharge_power = self.max_total_discharge_power;
                        } else if self.total_discharge_power < -self.max_total_charge_power {
                            self.total_discharge_power = -self.max_total_charge_power;
                        }
                        inv_state.discharge_power = self.total_discharge_power / num_inverters;
                    } else {
                        inv_state.discharge_power -= phase_power_val * 0.25;
                        inv_state.discharge_power += self.total_power * 0.1;
                    }
                }

                // 7. Clamp values
                if inv_state.discharge_power > inverter_config.max_discharge {
                    inv_state.discharge_power = inverter_config.max_discharge;
                } else if inv_state.discharge_power < -inverter_config.max_charge {
                    inv_state.discharge_power = -inverter_config.max_charge;
                }

                // 8. BMS Throttling
                if inv_state.battery_capacity > 95
                    && inv_state.discharge_power < 0.0
                    && inv_state.battery_power < (inv_state.discharge_power / -10.0)
                {
                    inv_state.discharge_power = 0.0;
                    self.assist_needed.insert(inverter_name.to_string(), true);
                }

                // 9. Grace period
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
                    } else if inv_state.discharge_power < -inverter_config.grace_charge_power {
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

    loop {
        match eventloop.poll().await {
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

                            if let Ok(val) = payload.trim().parse::<f64>() {
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
                sleep(Duration::from_secs(5)).await;
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
}
