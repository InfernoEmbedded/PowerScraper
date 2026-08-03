use crate::config::{
    BatteryControlInverter, BatteryControlPeriod, MqttBrokerConfig, SolaxBatteryControlConfig,
};
use crate::mqtt_helper::create_mqtt_client;
use chrono::{NaiveTime, Timelike, Utc, Datelike, TimeZone};
use rumqttc::{Event, Packet, QoS};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

fn parse_timezone_offset(tz_str: &str) -> chrono::FixedOffset {
    let mut seconds = 0;
    let tz_trimmed = tz_str.trim();
    
    let is_posix = if let Some(first_char) = tz_trimmed.chars().next() {
        first_char.is_alphabetic() && !tz_trimmed.to_uppercase().starts_with("UTC") && !tz_trimmed.to_uppercase().starts_with("GMT")
    } else {
        false
    };
    
    let search_str = if tz_trimmed.to_uppercase().starts_with("UTC") {
        &tz_trimmed[3..]
    } else if tz_trimmed.to_uppercase().starts_with("GMT") {
        &tz_trimmed[3..]
    } else {
        tz_trimmed
    };
    
    let mut sign = 1;
    let mut offset_str = "";
    
    if let Some(pos) = search_str.find(|c: char| c == '+' || c == '-' || c.is_ascii_digit()) {
        let remainder = &search_str[pos..];
        if remainder.starts_with('+') {
            sign = 1;
            offset_str = &remainder[1..];
        } else if remainder.starts_with('-') {
            sign = -1;
            offset_str = &remainder[1..];
        } else {
            sign = 1;
            offset_str = remainder;
        }
    }
    
    if is_posix {
        sign = -sign;
    }
    
    if !offset_str.is_empty() {
        if offset_str.contains(':') {
            let parts: Vec<&str> = offset_str.split(':').collect();
            if let Ok(hours) = parts[0].trim().parse::<i32>() {
                let minutes = parts.get(1).and_then(|m| m.trim().parse::<i32>().ok()).unwrap_or(0);
                seconds = sign * (hours * 3600 + minutes * 60);
            }
        } else if offset_str.contains('.') {
            if let Ok(val) = offset_str.parse::<f64>() {
                seconds = (sign as f64 * val * 3600.0) as i32;
            }
        } else {
            if let Ok(hours) = offset_str.trim().parse::<i32>() {
                seconds = sign * hours * 3600;
            }
        }
    }
    
    chrono::FixedOffset::east_opt(seconds).unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap())
}

pub fn get_timezone_offset(tz_str: Option<&str>) -> chrono::FixedOffset {
    if let Some(tz) = tz_str {
        parse_timezone_offset(tz)
    } else {
        use chrono::Offset;
        chrono::Local::now().offset().fix()
    }
}

#[derive(Debug, Clone, Default)]
pub struct InverterState {
    pub battery_capacity: u8,
    pub battery_power: f64,
    pub pv1_power: f64,
    pub pv2_power: f64,
    pub measured_power: f64,
    pub discharge_power: f64,
}

#[derive(Debug, Clone)]
pub struct HistoryRecord {
    pub timestamp: i64,
    pub topic: String,
    pub value: f64,
}

pub fn init_history_db(db_path: &str) -> Result<(), rusqlite::Error> {
    crate::database::init_history_db(db_path)
}

fn get_price_history(db_path: &str, topic: &str, since_timestamp: i64) -> Result<Vec<f64>, rusqlite::Error> {
    crate::database::get_price_history(db_path, topic, since_timestamp)
}

fn calculate_percentiles(mut values: Vec<f64>) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    let idx_30 = (n as f64 * 0.3).round() as usize;
    let idx_70 = (n as f64 * 0.7).round() as usize;
    let p30 = values[idx_30.min(n - 1)];
    let p70 = values[idx_70.min(n - 1)];
    (p30, p70)
}

pub fn calculate_price_thresholds(db_path: &str) -> Result<crate::web_server::PriceThresholds, rusqlite::Error> {
    let one_week_ago = Utc::now().timestamp() - (7 * 24 * 3600);
    
    // Import prices
    let import_prices = get_price_history(db_path, "tariff/import_price", one_week_ago).unwrap_or_default();
    let (import_30, import_70) = if import_prices.len() >= 10 {
        calculate_percentiles(import_prices)
    } else {
        (15.0, 35.0) // fallback
    };

    // Export prices
    let export_prices = get_price_history(db_path, "tariff/export_price", one_week_ago).unwrap_or_default();
    let (export_30, export_70) = if export_prices.len() >= 10 {
        calculate_percentiles(export_prices)
    } else {
        (5.0, 15.0) // fallback
    };

    Ok(crate::web_server::PriceThresholds {
        import_30,
        import_70,
        export_30,
        export_70,
    })
}

pub fn calculate_inferred_battery_capacity(db_path: &str, inverter_name: &str) -> Option<f64> {
    let capacity_topic = format!("{}/Battery Capacity", inverter_name);
    let power_topic = format!("{}/Battery Power", inverter_name);
    let ninety_days_ago = Utc::now().timestamp() - (90 * 24 * 3600);

    let mut rows = match crate::database::get_telemetry_history_multiple_topics(db_path, &capacity_topic, &power_topic, ninety_days_ago) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to fetch database rows for capacity inference: {}", e);
            return None;
        }
    };

    if rows.is_empty() {
        if let Ok(r) = crate::database::get_telemetry_history_multiple_topics(db_path, &capacity_topic, &power_topic, 0) {
            rows = r;
        }
    }

    // We process the records sequentially
    let mut current_soc: Option<f64> = None;
    let mut current_power: Option<f64> = None;
    let mut last_timestamp: Option<i64> = None;

    #[derive(PartialEq, Clone, Copy)]
    enum CycleDirection {
        Charging,
        Discharging,
    }

    let mut cycle_direction: Option<CycleDirection> = None;
    let mut cycle_start_soc: Option<f64> = None;
    let mut cycle_energy_wh: f64 = 0.0;
    
    let mut inferred_capacities: Vec<f64> = Vec::new();

    const EFFICIENCY_CHARGE: f64 = 0.95;
    const EFFICIENCY_DISCHARGE: f64 = 0.95;

    for (ts, topic, val) in rows {
        if topic == capacity_topic {
                current_soc = Some(val);
            } else if topic == power_topic {
                current_power = Some(val);
            }

            if let (Some(soc), Some(p), Some(last_ts)) = (current_soc, current_power, last_timestamp) {
                let dt = (ts - last_ts) as f64 / 3600.0; // hours
                if dt > 0.0 && dt < 0.2 { // max 12 mins gap
                    let energy_wh = p * dt;

                    if p < -50.0 {
                        // Charging
                        match cycle_direction {
                            Some(CycleDirection::Charging) => {
                                cycle_energy_wh += -energy_wh;
                            }
                            _ => {
                                // Finalize previous discharging cycle if any
                                if cycle_direction == Some(CycleDirection::Discharging) {
                                    if let Some(start_soc) = cycle_start_soc {
                                        let delta_soc = start_soc - soc;
                                        if delta_soc >= 20.0 {
                                            let cap = cycle_energy_wh / (10.0 * delta_soc * EFFICIENCY_DISCHARGE);
                                            if cap > 1.0 && cap < 100.0 {
                                                inferred_capacities.push(cap);
                                            }
                                        }
                                    }
                                }
                                // Start new charging cycle
                                cycle_direction = Some(CycleDirection::Charging);
                                cycle_start_soc = Some(soc);
                                cycle_energy_wh = -energy_wh;
                            }
                        }
                    } else if p > 50.0 {
                        // Discharging
                        match cycle_direction {
                            Some(CycleDirection::Discharging) => {
                                cycle_energy_wh += energy_wh;
                            }
                            _ => {
                                // Finalize previous charging cycle if any
                                if cycle_direction == Some(CycleDirection::Charging) {
                                    if let Some(start_soc) = cycle_start_soc {
                                        let delta_soc = soc - start_soc;
                                        if delta_soc >= 20.0 {
                                            let cap = (cycle_energy_wh * EFFICIENCY_CHARGE) / (10.0 * delta_soc);
                                            if cap > 1.0 && cap < 100.0 {
                                                inferred_capacities.push(cap);
                                            }
                                        }
                                    }
                                }
                                // Start new discharging cycle
                                cycle_direction = Some(CycleDirection::Discharging);
                                cycle_start_soc = Some(soc);
                                cycle_energy_wh = energy_wh;
                            }
                        }
                    } else {
                        // Near zero / Idle
                        if dt > 0.16 {
                            if let Some(dir) = cycle_direction {
                                if let Some(start_soc) = cycle_start_soc {
                                    match dir {
                                        CycleDirection::Charging => {
                                            let delta_soc = soc - start_soc;
                                            if delta_soc >= 20.0 {
                                                let cap = (cycle_energy_wh * EFFICIENCY_CHARGE) / (10.0 * delta_soc);
                                                if cap > 1.0 && cap < 100.0 {
                                                    inferred_capacities.push(cap);
                                                }
                                            }
                                        }
                                        CycleDirection::Discharging => {
                                            let delta_soc = start_soc - soc;
                                            if delta_soc >= 20.0 {
                                                let cap = cycle_energy_wh / (10.0 * delta_soc * EFFICIENCY_DISCHARGE);
                                                if cap > 1.0 && cap < 100.0 {
                                                    inferred_capacities.push(cap);
                                                }
                                            }
                                        }
                                    }
                                }
                                cycle_direction = None;
                                cycle_start_soc = None;
                                cycle_energy_wh = 0.0;
                            }
                        }
                    }
                } else if dt >= 0.2 {
                    // Large gap, finalize any active cycle
                    if let Some(dir) = cycle_direction {
                        if let Some(start_soc) = cycle_start_soc {
                            match dir {
                                CycleDirection::Charging => {
                                    let delta_soc = soc - start_soc;
                                    if delta_soc >= 20.0 {
                                        let cap = (cycle_energy_wh * EFFICIENCY_CHARGE) / (10.0 * delta_soc);
                                        if cap > 1.0 && cap < 100.0 {
                                            inferred_capacities.push(cap);
                                        }
                                    }
                                }
                                CycleDirection::Discharging => {
                                    let delta_soc = start_soc - soc;
                                    if delta_soc >= 20.0 {
                                        let cap = cycle_energy_wh / (10.0 * delta_soc * EFFICIENCY_DISCHARGE);
                                        if cap > 1.0 && cap < 100.0 {
                                            inferred_capacities.push(cap);
                                        }
                                    }
                                }
                            }
                        }
                        cycle_direction = None;
                        cycle_start_soc = None;
                        cycle_energy_wh = 0.0;
                    }
                }
            }
            last_timestamp = Some(ts);
    }

    // Finalize any remaining cycle at the end of the history
    if let (Some(dir), Some(start_soc), Some(soc)) = (cycle_direction, cycle_start_soc, current_soc) {
        match dir {
            CycleDirection::Charging => {
                let delta_soc = soc - start_soc;
                if delta_soc >= 20.0 {
                    let cap = (cycle_energy_wh * EFFICIENCY_CHARGE) / (10.0 * delta_soc);
                    if cap > 1.0 && cap < 100.0 {
                        inferred_capacities.push(cap);
                    }
                }
            }
            CycleDirection::Discharging => {
                let delta_soc = start_soc - soc;
                if delta_soc >= 20.0 {
                    let cap = cycle_energy_wh / (10.0 * delta_soc * EFFICIENCY_DISCHARGE);
                    if cap > 1.0 && cap < 100.0 {
                        inferred_capacities.push(cap);
                    }
                }
            }
        }
    }

    if inferred_capacities.is_empty() {
        None
    } else {
        let sum: f64 = inferred_capacities.iter().sum();
        let avg = sum / inferred_capacities.len() as f64;
        Some(avg)
    }
}

pub fn update_all_inferred_capacities(db_path: &str, inverters: &[String]) {
    println!("Calculating inferred battery capacities from SQLite history in parallel...");
    let mut results = Vec::new();
    std::thread::scope(|s| {
        let mut threads = Vec::new();
        for inv_name in inverters {
            let inv_name_clone = inv_name.clone();
            let handle = s.spawn(move || {
                let cap = calculate_inferred_battery_capacity(db_path, &inv_name_clone);
                (inv_name_clone, cap)
            });
            threads.push(handle);
        }
        for handle in threads {
            if let Ok(res) = handle.join() {
                results.push(res);
            }
        }
    });

    for (inv_name, cap_opt) in results {
        if let Some(cap) = cap_opt {
            println!("Inferred battery capacity for inverter [{}]: {:.2} kWh", inv_name, cap);
            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                let inv_status = status.inverters.entry(inv_name).or_default();
                inv_status.calculated_battery_capacity = Some(cap);
            }
        } else {
            println!("Not enough history/data to infer battery capacity for inverter [{}]", inv_name);
        }
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerManagerMode {
    Auto,
    ChargeBatteries,
    MaximumFeedin,
    SmartHeuristic,
    AdaptivePeakShaving,
    MpcOptimizer,
    MpcArbitrage,
    EvolvedHeuristic,
}

impl std::fmt::Display for PowerManagerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PowerManagerMode::Auto => write!(f, "Auto"),
            PowerManagerMode::ChargeBatteries => write!(f, "ChargeBatteries"),
            PowerManagerMode::MaximumFeedin => write!(f, "MaximumFeedin"),
            PowerManagerMode::SmartHeuristic => write!(f, "SmartHeuristic"),
            PowerManagerMode::AdaptivePeakShaving => write!(f, "AdaptivePeakShaving"),
            PowerManagerMode::MpcOptimizer => write!(f, "MpcOptimizer"),
            PowerManagerMode::MpcArbitrage => write!(f, "MpcArbitrage"),
            PowerManagerMode::EvolvedHeuristic => write!(f, "EvolvedHeuristic"),
        }
    }
}

impl std::str::FromStr for PowerManagerMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let clean = s.trim().to_lowercase().replace(['_', ' ', '-'], "");
        match clean.as_str() {
            "auto" => Ok(PowerManagerMode::Auto),
            "chargebatteries" | "charge" => Ok(PowerManagerMode::ChargeBatteries),
            "maximumfeedin" | "maxfeedin" | "maximum" | "max" => {
                Ok(PowerManagerMode::MaximumFeedin)
            }
            "smartheuristic" | "smart" | "heuristic" => {
                Ok(PowerManagerMode::SmartHeuristic)
            }
            "adaptivepeakshaving" | "adaptive" | "peakshaving" | "shaving" => {
                Ok(PowerManagerMode::AdaptivePeakShaving)
            }
            "mpcoptimizer" | "mpc" | "optimizer" => {
                Ok(PowerManagerMode::MpcOptimizer)
            }
            "mpcarbitrage" | "arbitrage" => {
                Ok(PowerManagerMode::MpcArbitrage)
            }
            "evolvedheuristic" | "evolved" => {
                Ok(PowerManagerMode::EvolvedHeuristic)
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
    pub tariff_manager: Arc<crate::tariff_manager::TariffManager>,
    last_regulation_update: std::time::Instant,
    pub db_path: String,
    cached_metrics: Option<(f64, f64, f64, f64, f64)>,
    last_metrics_update: Option<std::time::Instant>,
    phase_discharge_power: [f64; 16],
    commanded_powers: HashMap<String, i32>,
    pub low_capacity_state: HashMap<String, bool>,
}

impl PowerManager {
    pub fn get_today_solar_forecast_kwh(&self) -> f64 {
        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
        let now = chrono::Utc::now().with_timezone(&tz_offset);
        let start_dt = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_local_timezone(tz_offset).single().map(|dt| dt.timestamp()).unwrap_or(0);
        let end_dt = now.date_naive().and_hms_opt(23, 59, 59).unwrap().and_local_timezone(tz_offset).single().map(|dt| dt.timestamp()).unwrap_or(0);
        
        let mut expected_solar_kwh = 0.0;
        if let Ok(records) = crate::database::load_solar_forecast_range(&self.db_path, start_dt, end_dt) {
            let mut prev_ts = None;
            let mut sum_kwh = 0.0;
            for (ts, val) in records {
                if let Some(pts) = prev_ts {
                    let diff = (ts - pts) as f64 / 3600.0;
                    if diff > 0.0 && diff <= 2.0 {
                        sum_kwh += (val / 1000.0) * diff;
                    }
                }
                prev_ts = Some(ts);
            }
            expected_solar_kwh = sum_kwh;
        }
        expected_solar_kwh
    }

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
        let tariff_manager = Arc::new(crate::tariff_manager::TariffManager::new(
            config.tariff.clone(),
        ));

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
            tariff_manager,
            last_regulation_update: std::time::Instant::now() - std::time::Duration::from_secs(10),
            db_path: "config.db".to_string(),
            cached_metrics: None,
            last_metrics_update: None,
            phase_discharge_power: [0.0; 16],
            commanded_powers: HashMap::new(),
            low_capacity_state: HashMap::new(),
        }
    }

    fn get_period(&self) -> Option<&BatteryControlPeriod> {
        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
        let now_time = chrono::Utc::now().with_timezone(&tz_offset).time();
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

    fn check_low_capacity(&mut self, batteries: &[crate::battery_group::Battery], period: &BatteryControlPeriod) -> bool {
        let mut any_low = false;
        for b in batteries {
            let was_low = self.low_capacity_state.get(&b.name).copied().unwrap_or(false);
            let hyst = period.min_charge_hysteresis
                .or(self.config.min_charge_hysteresis)
                .unwrap_or(3) as f64;
                
            let is_low = if was_low {
                b.current_soc_pct < b.min_soc_pct + hyst
            } else {
                b.current_soc_pct < b.min_soc_pct
            };
            
            self.low_capacity_state.insert(b.name.clone(), is_low);
            if is_low {
                any_low = true;
            }
        }
        any_low
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
        if !self.inverters.contains_key(inverter_name) {
            self.inverters.insert(inverter_name.to_string(), InverterState::default());
        }

        let period_opt = self.get_period().cloned();
        if period_opt.is_none() && self.mode != PowerManagerMode::ChargeBatteries && self.mode != PowerManagerMode::MaximumFeedin {
            return None;
        }

        if !self.linked_batteries {
            let mut phase_sums = [0.0; 16];
            for (name, inv_cfg) in &self.config.inverter {
                if let Some(inv_state) = self.inverters.get(name) {
                    phase_sums[inv_cfg.phase] += inv_state.discharge_power;
                }
            }
            for p in 0..16 {
                self.phase_discharge_power[p] = phase_sums[p];
            }
        }

        let min_charge = self.config.period.values()
            .map(|p| p.min_charge)
            .min()
            .unwrap_or(10);

        let default_period = BatteryControlPeriod {
            start: "00:00:00".to_string(),
            end: "23:59:59".to_string(),
            min_charge,
            grid_charge: false,
            force_discharge: None,
            grace: false,
            prefer_battery: false,
            min_charge_hysteresis: None,
        };
        let period = period_opt.unwrap_or(default_period);

        let (negative_export_prevent, low_price_charge, low_price_threshold, high_price_discharge, high_price_threshold) =
            if let Some(crate::config::TariffConfig::Amber {
                negative_export_prevent,
                low_price_charge,
                low_price_threshold,
                high_price_discharge,
                high_price_threshold,
                ..
            }) = self.tariff_manager.config() {
                (*negative_export_prevent, *low_price_charge, *low_price_threshold, *high_price_discharge, *high_price_threshold)
            } else {
                (false, false, 15.0, false, 30.0)
            };

        // Build list of Battery structs
        let mut battery_map = HashMap::new();
        for (name, inv_cfg) in &self.config.inverter {
            if !inv_cfg.has_battery() {
                continue;
            }
            let state = self.inverters.get(name).cloned().unwrap_or_default();
            let cap_kwh = inv_cfg.battery_capacity.filter(|&c| c > 0.0).unwrap_or(13.8);
            let soc = state.battery_capacity as f64;
            let min_pct = if self.mode == PowerManagerMode::MaximumFeedin {
                inv_cfg.min_charge_pct.unwrap_or(10) as f64
            } else {
                period.min_charge.max(inv_cfg.min_charge_pct.unwrap_or(0)) as f64
            };
            let max_pct = inv_cfg.max_charge_pct.unwrap_or(95) as f64;

            let battery = crate::battery_group::Battery {
                name: name.clone(),
                capacity_wh: cap_kwh * 1000.0,
                current_soc_pct: soc,
                min_soc_pct: min_pct,
                max_soc_pct: max_pct,
                max_charge_power_w: inv_cfg.max_charge,
                max_discharge_power_w: inv_cfg.max_discharge,
            };
            battery_map.insert(name.clone(), battery);
        }

        let unique_phases: std::collections::HashSet<usize> = self.config.inverter.values().map(|inv| inv.phase).collect();
        let phase_count = unique_phases.len().max(1) as f64;

        if self.linked_batteries {
            let batteries_list: Vec<crate::battery_group::Battery> = battery_map.values().cloned().collect();
            let global_group = crate::battery_group::BatteryGroup::new(batteries_list);

            let max_total_charge = self.max_total_charge_power;
            let max_total_discharge = self.max_total_discharge_power;
            let total_pv: f64 = self.inverters.values().map(|inv| inv.pv1_power + inv.pv2_power).sum();
            let avg_soc = if !global_group.batteries.is_empty() {
                global_group.batteries.iter().map(|b| b.current_soc_pct * b.capacity_wh).sum::<f64>()
                    / global_group.batteries.iter().map(|b| b.capacity_wh).sum::<f64>()
            } else {
                0.0
            };
            let total_capacity_kwh = global_group.batteries.iter().map(|b| b.capacity_wh).sum::<f64>() / 1000.0;

            let any_low_capacity = self.check_low_capacity(&global_group.batteries, &period);

            let target_power = match self.mode {
                PowerManagerMode::ChargeBatteries => -max_total_charge,
                PowerManagerMode::MaximumFeedin => max_total_discharge,
                PowerManagerMode::Auto => {
                    let total_error = self.total_power - self.grid_target;
                    let now = std::time::Instant::now();
                    if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                        self.total_discharge_power += total_error * 0.1;
                    }
                    self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);

                    if period.grid_charge && any_low_capacity {
                        -max_total_charge
                    } else if any_low_capacity && period.prefer_battery {
                        (-total_pv).max(-max_total_charge)
                    } else {
                        self.total_discharge_power
                    }
                }
                PowerManagerMode::SmartHeuristic => {
                    let rates = self.tariff_manager.get_current_rates();
                    let import_rate = rates.import_rate;
                    let export_rate = rates.export_rate;

                    let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                    let now = chrono::Utc::now().with_timezone(&tz_offset);
                    let now_time = now.time();
                    let demand_window = get_demand_window(Some(&self.config));

                    let reserve_kwh = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                    let reserve_pct = ((reserve_kwh / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                    let reserve_pct = reserve_pct.max(period.min_charge);

                    let negative_export_triggered = negative_export_prevent && export_rate < 0.0;
                    
                    if import_rate < 0.0 || negative_export_triggered {
                        let group_max_charge_pct = global_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                        if avg_soc < group_max_charge_pct { -max_total_charge } else { 0.0 }
                    } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > reserve_pct as f64 {
                        max_total_discharge
                    } else if demand_window.map_or(false, |(start, _)| now_time.hour() >= 10 && now_time < start) && (low_price_charge && import_rate <= low_price_threshold || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0) && avg_soc < 85.0 {
                        -max_total_charge
                    } else {
                        let total_error = self.total_power - self.grid_target;
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.total_discharge_power += total_error * 0.1;
                        }
                        self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);
                        self.total_discharge_power
                    }
                }
                PowerManagerMode::EvolvedHeuristic => {
                    let rates = self.tariff_manager.get_current_rates();
                    let import_rate = rates.import_rate;
                    let export_rate = rates.export_rate;

                    let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                    let now = chrono::Utc::now().with_timezone(&tz_offset);
                    let now_time = now.time();
                    let hour = now.hour();
                    let demand_window = get_demand_window(Some(&self.config));

                    let month_key = chrono::Datelike::month(&now).to_string();
                    let eh_config = self.config.evolved_heuristic_monthly.as_ref()
                        .and_then(|m| m.get(&month_key).cloned())
                        .or_else(|| self.config.evolved_heuristic.clone())
                        .unwrap_or_default();

                    if import_rate < eh_config.neg_price_threshold {
                        let group_max_charge_pct = global_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                        if avg_soc < group_max_charge_pct { -max_total_charge } else { 0.0 }
                    } else if export_rate >= eh_config.tier2_export_dump_threshold || export_rate >= eh_config.export_dump_threshold {
                        let is_near_or_in_demand = if let Some((_, end)) = demand_window {
                            let hour_val = hour as i32;
                            let end_hour = end.hour() as i32;
                            hour_val >= 12 && hour_val < end_hour
                        } else {
                            hour >= 12 && hour < 21
                        };
                        let reserve_kwh = if export_rate >= eh_config.tier2_export_dump_threshold {
                            eh_config.tier2_dump_reserve
                        } else if is_near_or_in_demand {
                            eh_config.dump_reserve_demand
                        } else {
                            eh_config.dump_reserve_normal
                        };
                        let reserve_pct = ((reserve_kwh / total_capacity_kwh.max(1.0)) * 100.0).clamp(0.0, 100.0);

                        if avg_soc > reserve_pct { max_total_discharge } else { 0.0 }
                    } else if {
                        let is_pre_charge_window = if let Some((start, _)) = demand_window {
                            let start_h = start.hour();
                            let pc_start = eh_config.pre_charge_start_hour;
                            if pc_start <= start_h {
                                hour >= pc_start && hour < start_h
                            } else {
                                hour >= pc_start || hour < start_h
                            }
                        } else {
                            let pc_start = eh_config.pre_charge_start_hour;
                            if pc_start <= 15 {
                                hour >= pc_start && hour < 15
                            } else {
                                hour >= pc_start || hour < 15
                            }
                        };
                        let today_forecast_kwh = self.get_today_solar_forecast_kwh();
                        let effective_soc_limit = (eh_config.pre_charge_soc_limit * (1.0 - today_forecast_kwh * eh_config.forecast_solar_weight)).max(0.0);
                        is_pre_charge_window && import_rate < eh_config.pre_charge_price_threshold && (avg_soc / 100.0) < effective_soc_limit
                    } {
                        -max_total_charge
                    } else if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                        let orig_target = self.grid_target;
                        if eh_config.use_adaptive_shaving {
                            let monthly_peak = self.get_monthly_peak_draw();
                            self.grid_target = (monthly_peak - eh_config.adaptive_safety_buffer).max(0.0);
                        } else {
                            self.grid_target = 0.0;
                        }

                        let total_error = self.total_power - self.grid_target;
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.total_discharge_power += total_error * 0.1;
                        }
                        self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);

                        self.grid_target = orig_target;
                        self.total_discharge_power
                    } else {
                        let total_error = self.total_power - self.grid_target;
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.total_discharge_power += total_error * 0.1;
                        }
                        self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);
                        self.total_discharge_power
                    }
                }
                PowerManagerMode::AdaptivePeakShaving => {
                    let rates = self.tariff_manager.get_current_rates();
                    let import_rate = rates.import_rate;
                    let export_rate = rates.export_rate;

                    let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                    let now = chrono::Utc::now().with_timezone(&tz_offset);
                    let now_time = now.time();
                    let demand_window = get_demand_window(Some(&self.config));

                    let reserve_kwh = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                    let reserve_pct = ((reserve_kwh / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                    let reserve_pct = reserve_pct.max(period.min_charge);

                    let negative_export_triggered = negative_export_prevent && export_rate < 0.0;

                    if import_rate < 0.0 || negative_export_triggered {
                        let group_max_charge_pct = global_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                        if avg_soc < group_max_charge_pct { -max_total_charge } else { 0.0 }
                    } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > (reserve_pct + 10) as f64 {
                        max_total_discharge
                    } else if demand_window.map_or(false, |(start, _)| now_time.hour() >= 10 && now_time < start) && (low_price_charge && import_rate <= low_price_threshold || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0) && avg_soc < 85.0 {
                        -max_total_charge
                    } else if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                        let monthly_peak = self.get_monthly_peak_draw();
                        let orig_target = self.grid_target;
                        self.grid_target = monthly_peak;

                        let total_error = self.total_power - self.grid_target;
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.total_discharge_power += total_error * 0.1;
                        }
                        self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);

                        self.grid_target = orig_target;
                        self.total_discharge_power
                    } else {
                        let total_error = self.total_power - self.grid_target;
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.total_discharge_power += total_error * 0.1;
                        }
                        self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);
                        self.total_discharge_power
                    }
                }
                PowerManagerMode::MpcOptimizer => {
                    let rates = self.tariff_manager.get_current_rates();
                    let import_rate = rates.import_rate;
                    let export_rate = rates.export_rate;

                    let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                    let now = chrono::Utc::now().with_timezone(&tz_offset);
                    let now_time = now.time();
                    let demand_window = get_demand_window(Some(&self.config));

                    let (expected_solar, demand_needed, cheap_threshold, _, _) = self.get_persistence_metrics();

                    let required_reserve = demand_needed.min(total_capacity_kwh * 0.95);
                    let reserve_pct = ((required_reserve / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                    let reserve_pct = reserve_pct.max(period.min_charge);

                    let negative_export_triggered = negative_export_prevent && export_rate < 0.0;

                    if import_rate < 0.0 || negative_export_triggered {
                        let group_max_charge_pct = global_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                        if avg_soc < group_max_charge_pct { -max_total_charge } else { 0.0 }
                    } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > (reserve_pct + 10) as f64 {
                        max_total_discharge
                    } else if demand_window.map_or(false, |(start, _)| now_time < start) && ((avg_soc / 100.0 * total_capacity_kwh) + expected_solar) < required_reserve {
                        let is_cheap = if low_price_charge { import_rate <= low_price_threshold } else { import_rate < 12.0 || import_rate <= cheap_threshold } || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0;
                        if is_cheap {
                            -max_total_charge
                        } else {
                            let total_error = self.total_power - self.grid_target;
                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.total_discharge_power += total_error * 0.1;
                            }
                            self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);
                            self.total_discharge_power
                        }
                    } else {
                        let total_error = self.total_power - self.grid_target;
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.total_discharge_power += total_error * 0.1;
                        }
                        self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);
                        self.total_discharge_power
                    }
                }
                PowerManagerMode::MpcArbitrage => {
                    let rates = self.tariff_manager.get_current_rates();
                    let import_rate = rates.import_rate;
                    let export_rate = rates.export_rate;

                    let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                    let now = chrono::Utc::now().with_timezone(&tz_offset);
                    let now_time = now.time();
                    let demand_window = get_demand_window(Some(&self.config));

                    let (expected_solar, demand_needed, cheap_threshold, night_needed, night_avg_price) = self.get_persistence_metrics();

                    let demand_reserve = demand_needed.min(total_capacity_kwh * 0.95);
                    let total_needed = demand_needed + night_needed;
                    let required_reserve = total_needed.min(total_capacity_kwh * 0.95);

                    let current_charge = (avg_soc / 100.0) * total_capacity_kwh;
                    let is_cheap = if low_price_charge { import_rate <= low_price_threshold } else { import_rate < 12.0 || import_rate <= cheap_threshold } || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0;
                    let now_before_demand = demand_window.map_or(true, |(start, _)| now_time < start);

                    let target_reserve = if night_avg_price > import_rate * 1.10 { required_reserve } else { demand_reserve };
                    let reserve_pct = ((target_reserve / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                    let reserve_pct = reserve_pct.max(period.min_charge);

                    let negative_export_triggered = negative_export_prevent && export_rate < 0.0;

                    if import_rate < 0.0 || negative_export_triggered {
                        let group_max_charge_pct = global_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                        if avg_soc < group_max_charge_pct { -max_total_charge } else { 0.0 }
                    } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > (reserve_pct + 10) as f64 {
                        max_total_discharge
                    } else if now_before_demand && (current_charge + expected_solar) < target_reserve && is_cheap {
                        -max_total_charge
                    } else {
                        let total_error = self.total_power - self.grid_target;
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.total_discharge_power += total_error * 0.1;
                        }
                        self.total_discharge_power = self.total_discharge_power.clamp(-max_total_charge, max_total_discharge);
                        self.total_discharge_power
                    }
                }
            };

            let num_inverters = self.config_inverters_count as f64;
            global_group.calculate_and_constrain(
                target_power,
                &self.config.inverter,
                &mut self.inverters,
                &mut self.assist_needed,
                &mut self.commanded_powers,
                self.mode,
                &period,
                num_inverters,
            );

            let now = std::time::Instant::now();
            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                self.last_regulation_update = now;
            }
        } else {
            // Group by phase
            let mut phase_inverters: HashMap<usize, Vec<String>> = HashMap::new();
            for (name, inv_cfg) in &self.config.inverter {
                phase_inverters.entry(inv_cfg.phase).or_default().push(name.clone());
            }

            for (&p, name_list) in &phase_inverters {
                let batteries_list: Vec<crate::battery_group::Battery> = name_list.iter()
                    .filter_map(|n| battery_map.get(n).cloned())
                    .collect();
                let phase_group = crate::battery_group::BatteryGroup::new(batteries_list);

                let max_phase_charge: f64 = name_list.iter().filter_map(|n| self.config.inverter.get(n)).map(|i| i.max_charge).sum();
                let max_phase_discharge: f64 = name_list.iter().filter_map(|n| self.config.inverter.get(n)).map(|i| i.max_discharge).sum();
                let phase_pv: f64 = name_list.iter().filter_map(|n| self.inverters.get(n)).map(|inv| inv.pv1_power + inv.pv2_power).sum();
                let avg_soc = if !phase_group.batteries.is_empty() {
                    phase_group.batteries.iter().map(|b| b.current_soc_pct * b.capacity_wh).sum::<f64>()
                        / phase_group.batteries.iter().map(|b| b.capacity_wh).sum::<f64>()
                } else {
                    0.0
                };
                let total_capacity_kwh = phase_group.batteries.iter().map(|b| b.capacity_wh).sum::<f64>() / 1000.0;

                let any_low_capacity = self.check_low_capacity(&phase_group.batteries, &period);

                let use_total = name_list.iter().any(|n| {
                    self.config.inverter.get(n).map_or(false, |cfg| cfg.use_total_power)
                });
                let phase_error = if use_total {
                    self.total_power - self.grid_target
                } else {
                    self.phase_power[p] - (self.grid_target / phase_count)
                };

                let target_power = match self.mode {
                    PowerManagerMode::ChargeBatteries => -max_phase_charge,
                    PowerManagerMode::MaximumFeedin => max_phase_discharge,
                    PowerManagerMode::Auto => {
                        let now = std::time::Instant::now();
                        if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                            self.phase_discharge_power[p] += phase_error * 0.25;
                        }
                        self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);

                        if period.grid_charge && any_low_capacity {
                            -max_phase_charge
                        } else if any_low_capacity && period.prefer_battery {
                            (-phase_pv).max(-max_phase_charge)
                        } else {
                            self.phase_discharge_power[p]
                        }
                    }
                    PowerManagerMode::SmartHeuristic => {
                        let rates = self.tariff_manager.get_current_rates();
                        let import_rate = rates.import_rate;
                        let export_rate = rates.export_rate;

                        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                        let now = chrono::Utc::now().with_timezone(&tz_offset);
                        let now_time = now.time();
                        let demand_window = get_demand_window(Some(&self.config));

                        let reserve_kwh = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                        let reserve_pct = ((reserve_kwh / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                        let reserve_pct = reserve_pct.max(period.min_charge);

                        let negative_export_prevent = if let Some(crate::config::TariffConfig::Amber { negative_export_prevent, .. }) = self.tariff_manager.config() { *negative_export_prevent } else { false };
                        let low_price_charge = if let Some(crate::config::TariffConfig::Amber { low_price_charge, .. }) = self.tariff_manager.config() { *low_price_charge } else { false };
                        let low_price_threshold = if let Some(crate::config::TariffConfig::Amber { low_price_threshold, .. }) = self.tariff_manager.config() { *low_price_threshold } else { 15.0 };
                        let high_price_discharge = if let Some(crate::config::TariffConfig::Amber { high_price_discharge, .. }) = self.tariff_manager.config() { *high_price_discharge } else { false };
                        let high_price_threshold = if let Some(crate::config::TariffConfig::Amber { high_price_threshold, .. }) = self.tariff_manager.config() { *high_price_threshold } else { 30.0 };

                        let negative_export_triggered = negative_export_prevent && export_rate < 0.0;
                        
                        if import_rate < 0.0 || negative_export_triggered {
                            let group_max_charge_pct = phase_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                            if avg_soc < group_max_charge_pct { -max_phase_charge } else { 0.0 }
                        } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > reserve_pct as f64 {
                            max_phase_discharge
                        } else if demand_window.map_or(false, |(start, _)| now_time.hour() >= 10 && now_time < start) && (low_price_charge && import_rate <= low_price_threshold || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0) && avg_soc < 85.0 {
                            -max_phase_charge
                        } else {
                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.phase_discharge_power[p] += phase_error * 0.25;
                            }
                            self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);
                            self.phase_discharge_power[p]
                        }
                    }
                    PowerManagerMode::EvolvedHeuristic => {
                        let rates = self.tariff_manager.get_current_rates();
                        let import_rate = rates.import_rate;
                        let export_rate = rates.export_rate;

                        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                        let now = chrono::Utc::now().with_timezone(&tz_offset);
                        let now_time = now.time();
                        let hour = now.hour();
                        let demand_window = get_demand_window(Some(&self.config));

                        let month_key = chrono::Datelike::month(&now).to_string();
                        let eh_config = self.config.evolved_heuristic_monthly.as_ref()
                            .and_then(|m| m.get(&month_key).cloned())
                            .or_else(|| self.config.evolved_heuristic.clone())
                            .unwrap_or_default();

                        if import_rate < eh_config.neg_price_threshold {
                            let group_max_charge_pct = phase_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                            if avg_soc < group_max_charge_pct { -max_phase_charge } else { 0.0 }
                        } else if export_rate >= eh_config.tier2_export_dump_threshold || export_rate >= eh_config.export_dump_threshold {
                            let is_near_or_in_demand = if let Some((_, end)) = demand_window {
                                let hour_val = hour as i32;
                                let end_hour = end.hour() as i32;
                                hour_val >= 12 && hour_val < end_hour
                            } else {
                                hour >= 12 && hour < 21
                            };
                            let reserve_kwh = if export_rate >= eh_config.tier2_export_dump_threshold {
                                eh_config.tier2_dump_reserve
                            } else if is_near_or_in_demand {
                                eh_config.dump_reserve_demand
                            } else {
                                eh_config.dump_reserve_normal
                            };
                            let reserve_pct = ((reserve_kwh / total_capacity_kwh.max(1.0)) * 100.0).clamp(0.0, 100.0);

                            if avg_soc > reserve_pct { max_phase_discharge } else { 0.0 }
                        } else if {
                            let is_pre_charge_window = if let Some((start, _)) = demand_window {
                                let start_h = start.hour();
                                let pc_start = eh_config.pre_charge_start_hour;
                                if pc_start <= start_h {
                                    hour >= pc_start && hour < start_h
                                } else {
                                    hour >= pc_start || hour < start_h
                                }
                            } else {
                                let pc_start = eh_config.pre_charge_start_hour;
                                if pc_start <= 15 {
                                    hour >= pc_start && hour < 15
                                } else {
                                    hour >= pc_start || hour < 15
                                }
                            };
                            let today_forecast_kwh = self.get_today_solar_forecast_kwh();
                            let effective_soc_limit = (eh_config.pre_charge_soc_limit * (1.0 - today_forecast_kwh * eh_config.forecast_solar_weight)).max(0.0);
                            is_pre_charge_window && import_rate < eh_config.pre_charge_price_threshold && (avg_soc / 100.0) < effective_soc_limit
                        } {
                            -max_phase_charge
                        } else if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                            let orig_target = self.grid_target;
                            if eh_config.use_adaptive_shaving {
                                let monthly_peak = self.get_monthly_peak_draw();
                                self.grid_target = (monthly_peak - eh_config.adaptive_safety_buffer).max(0.0);
                            } else {
                                self.grid_target = 0.0;
                            }

                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.phase_discharge_power[p] += phase_error * 0.25;
                            }
                            self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);

                            self.grid_target = orig_target;
                            self.phase_discharge_power[p]
                        } else {
                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.phase_discharge_power[p] += phase_error * 0.25;
                            }
                            self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);
                            self.phase_discharge_power[p]
                        }
                    }
                    PowerManagerMode::AdaptivePeakShaving => {
                        let rates = self.tariff_manager.get_current_rates();
                        let import_rate = rates.import_rate;
                        let export_rate = rates.export_rate;

                        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                        let now = chrono::Utc::now().with_timezone(&tz_offset);
                        let now_time = now.time();
                        let demand_window = get_demand_window(Some(&self.config));

                        let reserve_kwh = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                        let reserve_pct = ((reserve_kwh / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                        let reserve_pct = reserve_pct.max(period.min_charge);

                        let negative_export_prevent = if let Some(crate::config::TariffConfig::Amber { negative_export_prevent, .. }) = self.tariff_manager.config() { *negative_export_prevent } else { false };
                        let high_price_discharge = if let Some(crate::config::TariffConfig::Amber { high_price_discharge, .. }) = self.tariff_manager.config() { *high_price_discharge } else { false };
                        let high_price_threshold = if let Some(crate::config::TariffConfig::Amber { high_price_threshold, .. }) = self.tariff_manager.config() { *high_price_threshold } else { 30.0 };

                        let negative_export_triggered = negative_export_prevent && export_rate < 0.0;

                        if import_rate < 0.0 || negative_export_triggered {
                            let group_max_charge_pct = phase_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                            if avg_soc < group_max_charge_pct { -max_phase_charge } else { 0.0 }
                        } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > (reserve_pct + 10) as f64 {
                            max_phase_discharge
                        } else if demand_window.map_or(false, |(start, _)| now_time.hour() >= 10 && now_time < start) && (low_price_charge && import_rate <= low_price_threshold || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0) && avg_soc < 85.0 {
                            -max_phase_charge
                        } else if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                            let monthly_peak = self.get_monthly_peak_draw();
                            let orig_target = self.grid_target;
                            self.grid_target = monthly_peak;

                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.phase_discharge_power[p] += phase_error * 0.25;
                            }
                            self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);

                            self.grid_target = orig_target;
                            self.phase_discharge_power[p]
                        } else {
                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.phase_discharge_power[p] += phase_error * 0.25;
                            }
                            self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);
                            self.phase_discharge_power[p]
                        }
                    }
                    PowerManagerMode::MpcOptimizer => {
                        let rates = self.tariff_manager.get_current_rates();
                        let import_rate = rates.import_rate;
                        let export_rate = rates.export_rate;

                        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                        let now = chrono::Utc::now().with_timezone(&tz_offset);
                        let now_time = now.time();
                        let demand_window = get_demand_window(Some(&self.config));

                        let (expected_solar, demand_needed, cheap_threshold, _, _) = self.get_persistence_metrics();

                        let required_reserve = demand_needed.min(total_capacity_kwh * 0.95);
                        let reserve_pct = ((required_reserve / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                        let reserve_pct = reserve_pct.max(period.min_charge);

                        let negative_export_prevent = if let Some(crate::config::TariffConfig::Amber { negative_export_prevent, .. }) = self.tariff_manager.config() { *negative_export_prevent } else { false };
                        let low_price_charge = if let Some(crate::config::TariffConfig::Amber { low_price_charge, .. }) = self.tariff_manager.config() { *low_price_charge } else { false };
                        let low_price_threshold = if let Some(crate::config::TariffConfig::Amber { low_price_threshold, .. }) = self.tariff_manager.config() { *low_price_threshold } else { 15.0 };
                        let high_price_discharge = if let Some(crate::config::TariffConfig::Amber { high_price_discharge, .. }) = self.tariff_manager.config() { *high_price_discharge } else { false };
                        let high_price_threshold = if let Some(crate::config::TariffConfig::Amber { high_price_threshold, .. }) = self.tariff_manager.config() { *high_price_threshold } else { 30.0 };

                        let negative_export_triggered = negative_export_prevent && export_rate < 0.0;

                        if import_rate < 0.0 || negative_export_triggered {
                            let group_max_charge_pct = phase_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                            if avg_soc < group_max_charge_pct { -max_phase_charge } else { 0.0 }
                        } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > (reserve_pct + 10) as f64 {
                            max_phase_discharge
                        } else if demand_window.map_or(false, |(start, _)| now_time < start) && ((avg_soc / 100.0 * total_capacity_kwh) + expected_solar) < required_reserve {
                            let is_cheap = if low_price_charge { import_rate <= low_price_threshold } else { import_rate < 12.0 || import_rate <= cheap_threshold } || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0;
                            if is_cheap {
                                -max_phase_charge
                            } else {
                                let now = std::time::Instant::now();
                                if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                    self.phase_discharge_power[p] += phase_error * 0.25;
                                }
                                self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);
                                self.phase_discharge_power[p]
                            }
                        } else {
                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.phase_discharge_power[p] += phase_error * 0.25;
                            }
                            self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);
                            self.phase_discharge_power[p]
                        }
                    }
                    PowerManagerMode::MpcArbitrage => {
                        let rates = self.tariff_manager.get_current_rates();
                        let import_rate = rates.import_rate;
                        let export_rate = rates.export_rate;

                        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
                        let now = chrono::Utc::now().with_timezone(&tz_offset);
                        let now_time = now.time();
                        let demand_window = get_demand_window(Some(&self.config));

                        let (expected_solar, demand_needed, cheap_threshold, night_needed, night_avg_price) = self.get_persistence_metrics();

                        let demand_reserve = demand_needed.min(total_capacity_kwh * 0.95);
                        let total_needed = demand_needed + night_needed;
                        let required_reserve = total_needed.min(total_capacity_kwh * 0.95);

                        let current_charge = (avg_soc / 100.0) * total_capacity_kwh;
                        let is_cheap = if low_price_charge { import_rate <= low_price_threshold } else { import_rate < 12.0 || import_rate <= cheap_threshold } || self.config.demand.as_ref().map_or(0.0, |d| d.rate) > 0.0;
                        let now_before_demand = demand_window.map_or(true, |(start, _)| now_time < start);

                        let target_reserve = if night_avg_price > import_rate * 1.10 { required_reserve } else { demand_reserve };
                        let reserve_pct = ((target_reserve / total_capacity_kwh.max(1.0)) * 100.0) as u8;
                        let reserve_pct = reserve_pct.max(period.min_charge);

                        let negative_export_prevent = if let Some(crate::config::TariffConfig::Amber { negative_export_prevent, .. }) = self.tariff_manager.config() { *negative_export_prevent } else { false };
                        let high_price_discharge = if let Some(crate::config::TariffConfig::Amber { high_price_discharge, .. }) = self.tariff_manager.config() { *high_price_discharge } else { false };
                        let high_price_threshold = if let Some(crate::config::TariffConfig::Amber { high_price_threshold, .. }) = self.tariff_manager.config() { *high_price_threshold } else { 30.0 };

                        let negative_export_triggered = negative_export_prevent && export_rate < 0.0;

                        if import_rate < 0.0 || negative_export_triggered {
                            let group_max_charge_pct = phase_group.batteries.iter().map(|b| b.max_soc_pct).fold(0.0_f64, |a, b| a.max(b));
                            if avg_soc < group_max_charge_pct { -max_phase_charge } else { 0.0 }
                        } else if high_price_discharge && export_rate >= high_price_threshold && avg_soc > (reserve_pct + 10) as f64 {
                            max_phase_discharge
                        } else if now_before_demand && (current_charge + expected_solar) < target_reserve && is_cheap {
                            -max_phase_charge
                        } else {
                            let now = std::time::Instant::now();
                            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                                self.phase_discharge_power[p] += phase_error * 0.25;
                            }
                            self.phase_discharge_power[p] = self.phase_discharge_power[p].clamp(-max_phase_charge, max_phase_discharge);
                            self.phase_discharge_power[p]
                        }
                    }
                };

                phase_group.calculate_and_constrain(
                    target_power,
                    &self.config.inverter,
                    &mut self.inverters,
                    &mut self.assist_needed,
                    &mut self.commanded_powers,
                    self.mode,
                    &period,
                    phase_count,
                );
            }

            let now = std::time::Instant::now();
            if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                self.last_regulation_update = now;
            }
        }

        self.commanded_powers.get(inverter_name).copied()
    }

    pub async fn command_group(
        &mut self,
        device_name: &str,
        mqtt_client: &rumqttc::AsyncClient,
        base_topic: &str,
    ) {
        // Publish current mode and grid_target status
        let mode_topic = format!("{}/power_manager/mode", base_topic);
        let _ = mqtt_client.publish(&mode_topic, rumqttc::QoS::AtMostOnce, true, self.mode.to_string()).await;
        let target_topic = format!("{}/power_manager/grid_target", base_topic);
        let _ = mqtt_client.publish(&target_topic, rumqttc::QoS::AtMostOnce, true, self.grid_target.to_string()).await;

        // Run control logic to update state/commands
        self.evaluate_and_command(device_name);

        let period_opt = self.get_period().cloned();
        let min_charge = self.config.period.values()
            .map(|p| p.min_charge)
            .min()
            .unwrap_or(10);
        let default_period = BatteryControlPeriod {
            start: "00:00:00".to_string(),
            end: "23:59:59".to_string(),
            min_charge,
            grid_charge: false,
            force_discharge: None,
            grace: false,
            prefer_battery: false,
            min_charge_hysteresis: None,
        };
        let period = period_opt.unwrap_or(default_period);

        if self.linked_batteries {
            // Build global group
            let mut batteries = Vec::new();
            for (name, inv_cfg) in &self.config.inverter {
                if !inv_cfg.has_battery() {
                    continue;
                }
                let state = self.inverters.get(name).cloned().unwrap_or_default();
                let calc_cap = if let Ok(status) = crate::web_server::get_system_status().lock() {
                    status.inverters.get(name).and_then(|i| i.calculated_battery_capacity)
                } else {
                    None
                };
                let cap_kwh = calc_cap
                    .or_else(|| inv_cfg.battery_capacity.filter(|&c| c > 0.0))
                    .unwrap_or(13.8);
                let soc = state.battery_capacity as f64;
                let min_pct = if self.mode == PowerManagerMode::MaximumFeedin {
                    inv_cfg.min_charge_pct.unwrap_or(10) as f64
                } else {
                    period.min_charge.max(inv_cfg.min_charge_pct.unwrap_or(0)) as f64
                };
                let max_pct = inv_cfg.max_charge_pct.unwrap_or(95) as f64;

                let battery = crate::battery_group::Battery {
                    name: name.clone(),
                    capacity_wh: cap_kwh * 1000.0,
                    current_soc_pct: soc,
                    min_soc_pct: min_pct,
                    max_soc_pct: max_pct,
                    max_charge_power_w: inv_cfg.max_charge,
                    max_discharge_power_w: inv_cfg.max_discharge,
                };
                batteries.push(battery);
            }
            let global_group = crate::battery_group::BatteryGroup::new(batteries);

            // Re-calculate target power
            let max_total_charge = self.max_total_charge_power;
            let max_total_discharge = self.max_total_discharge_power;
            let total_pv: f64 = self.inverters.values().map(|inv| inv.pv1_power + inv.pv2_power).sum();
            let any_low_capacity = self.check_low_capacity(&global_group.batteries, &period);

            let target_power = match self.mode {
                PowerManagerMode::ChargeBatteries => -max_total_charge,
                PowerManagerMode::MaximumFeedin => max_total_discharge,
                PowerManagerMode::Auto => {
                    if period.grid_charge && any_low_capacity {
                        -max_total_charge
                    } else if any_low_capacity && period.prefer_battery {
                        (-total_pv).max(-max_total_charge)
                    } else {
                        self.total_discharge_power
                    }
                }
                _ => self.total_discharge_power,
            };

            let num_inverters = self.config_inverters_count as f64;
            global_group.command_inverters(
                target_power,
                mqtt_client,
                base_topic,
                &self.config.inverter,
                &mut self.inverters,
                &mut self.assist_needed,
                &mut self.commanded_powers,
                self.mode,
                &period,
                num_inverters,
            ).await;
        } else {
            // Unlinked
            if let Some(inv_cfg) = self.config.inverter.get(device_name) {
                if !inv_cfg.has_battery() {
                    return;
                }
                let phase = inv_cfg.phase;
                let phase_inverters: Vec<String> = self.config.inverter.iter()
                    .filter(|(_, cfg)| cfg.phase == phase)
                    .map(|(name, _)| name.clone())
                    .collect();

                let mut phase_batteries = Vec::new();
                for name in &phase_inverters {
                    if let Some(cfg) = self.config.inverter.get(name) {
                        if !cfg.has_battery() {
                            continue;
                        }
                        let state = self.inverters.get(name).cloned().unwrap_or_default();
                        let calc_cap = if let Ok(status) = crate::web_server::get_system_status().lock() {
                            status.inverters.get(name).and_then(|i| i.calculated_battery_capacity)
                        } else {
                            None
                        };
                        let cap_kwh = calc_cap
                            .or_else(|| cfg.battery_capacity.filter(|&c| c > 0.0))
                            .unwrap_or(13.8);
                        let soc = state.battery_capacity as f64;
                        let min_pct = if self.mode == PowerManagerMode::MaximumFeedin {
                            cfg.min_charge_pct.unwrap_or(10) as f64
                        } else {
                            period.min_charge.max(cfg.min_charge_pct.unwrap_or(0)) as f64
                        };
                        let max_pct = cfg.max_charge_pct.unwrap_or(95) as f64;

                        let battery = crate::battery_group::Battery {
                            name: name.clone(),
                            capacity_wh: cap_kwh * 1000.0,
                            current_soc_pct: soc,
                            min_soc_pct: min_pct,
                            max_soc_pct: max_pct,
                            max_charge_power_w: cfg.max_charge,
                            max_discharge_power_w: cfg.max_discharge,
                        };
                        phase_batteries.push(battery);
                    }
                }
                let phase_group = crate::battery_group::BatteryGroup::new(phase_batteries);

                // Re-calculate target power
                let max_phase_charge = inv_cfg.max_charge;
                let max_phase_discharge = inv_cfg.max_discharge;

                let target_power = match self.mode {
                    PowerManagerMode::ChargeBatteries => -max_phase_charge,
                    PowerManagerMode::MaximumFeedin => max_phase_discharge,
                    _ => self.phase_discharge_power[phase],
                };

                let phase_count = phase_inverters.len() as f64;
                phase_group.command_inverters(
                    target_power,
                    mqtt_client,
                    base_topic,
                    &self.config.inverter,
                    &mut self.inverters,
                    &mut self.assist_needed,
                    &mut self.commanded_powers,
                    self.mode,
                    &period,
                    phase_count,
                ).await;
            }
        }
    }

    #[allow(dead_code)]
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

        let min_limit = period.min_charge.max(in_cfg.min_charge_pct.unwrap_or(0));
        if battery_capacity <= min_limit && power > 0.0 {
            power = 0.0;
        }

        let max_limit = in_cfg.max_charge_pct.unwrap_or(100);
        if battery_capacity >= max_limit && power < 0.0 {
            power = 0.0;
        }

        -power as i32
    }

    pub fn calculate_aggregates(&self) -> HashMap<String, f64> {
        let mut aggregates = HashMap::new();

        let total_solar_production: f64 = self
            .inverters
            .values()
            .map(|inv| inv.pv1_power + inv.pv2_power)
            .sum();

        let total_charging: f64 = self
            .inverters
            .values()
            .map(|inv| {
                if inv.battery_power < 0.0 {
                    -inv.battery_power
                } else {
                    0.0
                }
            })
            .sum();

        let total_discharging: f64 = self
            .inverters
            .values()
            .map(|inv| {
                if inv.battery_power > 0.0 {
                    inv.battery_power
                } else {
                    0.0
                }
            })
            .sum();

        let total_battery_power: f64 = self.inverters.values().map(|inv| inv.battery_power).sum();

        let grid_power = self.total_power;

        // total_consumption = solar + grid + battery_power
        // (where battery_power is positive when discharging, negative when charging)
        let total_consumption =
            (total_solar_production + grid_power + total_battery_power).max(0.0);

        let total_grid_power_used_for_charging = if grid_power > 0.0 && total_charging > 0.0 {
            grid_power.min(total_charging)
        } else {
            0.0
        };

        aggregates.insert("Total Solar Production".to_string(), total_solar_production);
        aggregates.insert(
            "Total Grid Power Used for Charging".to_string(),
            total_grid_power_used_for_charging,
        );
        aggregates.insert("Total Consumption".to_string(), total_consumption);
        aggregates.insert("Total Charging".to_string(), total_charging);
        aggregates.insert("Total Discharging".to_string(), total_discharging);
        aggregates.insert("Usage".to_string(), total_consumption);

        // Power Budget Calculations
        let power_budget = crate::power_budget::calculate_power_budget(total_solar_production, total_consumption);
        aggregates.insert("Power Budget".to_string(), power_budget);

        // Power Budget with Charging Calculations
        let power_budget_with_charging = crate::power_budget::calculate_power_budget_with_charging(
            total_solar_production,
            total_charging,
            total_consumption,
        );
        aggregates.insert("Power Budget with charging".to_string(), power_budget_with_charging);
        aggregates.insert("Power Budget with Charging".to_string(), power_budget_with_charging);

        aggregates
    }

    fn get_monthly_peak_draw(&self) -> f64 {
        let demand_window = get_demand_window(Some(&self.config));
        if demand_window.is_none() {
            return 0.0;
        }
        let (demand_start, demand_end) = demand_window.unwrap();
        let mains_source = self.config.source.as_deref().unwrap_or("MainsMeter");

        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
        let now = chrono::Utc::now().with_timezone(&tz_offset);
        let start_of_month = match tz_offset.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0) {
            chrono::LocalResult::Single(t) => t,
            _ => return 0.0,
        };
        let start_epoch = start_of_month.timestamp();

        let topic = format!("{}/Total system power", mains_source);
        let records = match crate::database::get_telemetry_history(&self.db_path, &topic, start_epoch) {
            Ok(r) => r,
            Err(_) => return 0.0,
        };

        let mut peak = 0.0;
        for (ts, val) in records {
            let dt = chrono::Utc.timestamp_opt(ts, 0)
                .single()
                .map(|utc| utc.with_timezone(&tz_offset))
                .unwrap_or_else(|| chrono::Utc::now().with_timezone(&tz_offset));

            let dt_time = dt.time();
            if is_time_in_window(dt_time, demand_start, demand_end) && val > peak {
                peak = val;
            }
        }
        peak
    }

    pub fn clear_metrics_cache(&mut self) {
        self.cached_metrics = None;
        self.last_metrics_update = None;
    }

    fn get_persistence_metrics(&mut self) -> (f64, f64, f64, f64, f64) {
        if let Some(last_update) = self.last_metrics_update {
            if last_update.elapsed() < std::time::Duration::from_secs(300) {
                if let Some(cached) = self.cached_metrics {
                    return cached;
                }
            }
        }
        let metrics = self.compute_persistence_metrics();
        self.cached_metrics = Some(metrics);
        self.last_metrics_update = Some(std::time::Instant::now());
        metrics
    }

    fn compute_persistence_metrics(&self) -> (f64, f64, f64, f64, f64) {
        let demand_window = get_demand_window(Some(&self.config));
        let mains_source = self.config.source.as_deref().unwrap_or("MainsMeter");

        let now = chrono::Local::now().timestamp();
        let since = now - 86400; // 24 hours ago

        let rows = match crate::database::get_all_telemetry_in_range(&self.db_path, since, now) {
            Ok(r) => r,
            Err(_) => return (0.0, 0.0, 12.0, 0.0, 30.0),
        };

        use std::collections::BTreeMap;
        struct RawGroup {
            solar: f64,
            load: Option<f64>,
            import_price: Option<f64>,
        }
        let mut groups: BTreeMap<i64, RawGroup> = BTreeMap::new();

        for (ts, topic, val) in rows {
            let entry = groups.entry(ts).or_insert(RawGroup {
                solar: 0.0,
                load: None,
                import_price: None,
            });

            if topic.ends_with("/PV1 Power") || topic.ends_with("/PV2 Power") || topic.contains("/Input 1 Power") || topic.contains("/Input 2 Power") {
                entry.solar += val;
            } else if topic == format!("{}/Total system power", mains_source) {
                entry.load = Some(val);
            } else if topic == "tariff/import_price" {
                entry.import_price = Some(val);
            }
        }

        let keys: Vec<i64> = groups.keys().cloned().collect();
        if keys.len() < 2 {
            return (0.0, 0.0, 12.0, 0.0, 30.0);
        }

        let mut expected_solar_kwh = 0.0;
        let mut forecast_used = false;

        let tz_offset = get_timezone_offset(self.config.timezone.as_deref());
        let now = chrono::Utc::now().with_timezone(&tz_offset);
        let now_ts = now.timestamp();
        let demand_start_time = demand_window.map(|(start, _)| start).unwrap_or_else(|| NaiveTime::from_hms_opt(17, 0, 0).unwrap());
        let mut end_dt = now.date_naive().and_time(demand_start_time).and_local_timezone(tz_offset).single().map(|dt| dt.timestamp()).unwrap_or(now_ts);
        
        let start_ts = if now_ts >= end_dt {
            let tomorrow = now + chrono::Duration::days(1);
            end_dt = tomorrow.date_naive().and_time(demand_start_time).and_local_timezone(tz_offset).single().map(|dt| dt.timestamp()).unwrap_or(now_ts);
            tomorrow.date_naive().and_time(NaiveTime::from_hms_opt(10, 0, 0).unwrap()).and_local_timezone(tz_offset).single().map(|dt| dt.timestamp()).unwrap_or(now_ts)
        } else {
            now_ts
        };

        if let Ok(records) = crate::database::load_solar_forecast_range(&self.db_path, start_ts, end_dt) {
            let mut prev_ts = None;
            let mut sum_kwh = 0.0;
            let mut points = 0;
            for (ts, val) in records {
                if let Some(pts) = prev_ts {
                    let diff = (ts - pts) as f64 / 3600.0;
                    if diff > 0.0 && diff <= 2.0 {
                        sum_kwh += (val / 1000.0) * diff;
                        points += 1;
                    }
                } else {
                    let diff = (ts - start_ts) as f64 / 3600.0;
                    if diff > 0.0 && diff <= 2.0 {
                        sum_kwh += (val / 1000.0) * diff;
                        points += 1;
                    }
                }
                prev_ts = Some(ts);
            }
            if points > 0 {
                expected_solar_kwh = sum_kwh;
                forecast_used = true;
                println!("Using Open-Meteo weather forecast for solar predictions: {:.2} kWh", expected_solar_kwh);
            }
        }

        let mut demand_energy_needed_kwh = 0.0;
        let mut import_prices = Vec::new();

        let night_start = demand_window.map(|(_, end)| end).unwrap_or_else(|| NaiveTime::from_hms_opt(20, 0, 0).unwrap());
        let night_end = NaiveTime::from_hms_opt(6, 0, 0).unwrap();
        let mut night_energy_needed_kwh = 0.0;
        let mut night_prices = Vec::new();

        for i in 0..keys.len() - 1 {
            let ts = keys[i];
            let next_ts = keys[i + 1];
            let duration_hours = (next_ts - ts) as f64 / 3600.0;
            if duration_hours > 2.0 {
                continue; // ignore huge gaps
            }

            let g = &groups[&ts];
            let solar = g.solar;
            let load = g.load.unwrap_or(0.0);

            let dt = chrono::Utc.timestamp_opt(ts, 0)
                .single()
                .map(|utc| utc.with_timezone(&tz_offset))
                .unwrap_or_else(|| chrono::Utc::now().with_timezone(&tz_offset));
            let dt_time = dt.time();

            if !forecast_used && demand_window.map_or(false, |(start, _)| dt_time < start) {
                expected_solar_kwh += (solar / 1000.0) * duration_hours;
            }

            if demand_window.map_or(false, |(start, end)| is_time_in_window(dt_time, start, end)) {
                let net_power = load - solar;
                if net_power > 0.0 {
                    demand_energy_needed_kwh += (net_power / 1000.0) * duration_hours;
                }
            }

            if is_time_in_window(dt_time, night_start, night_end) {
                let net_power = load - solar;
                if net_power > 0.0 {
                    night_energy_needed_kwh += (net_power / 1000.0) * duration_hours;
                }
                if let Some(price) = g.import_price {
                    night_prices.push(price);
                }
            }

            if let Some(price) = g.import_price {
                import_prices.push(price);
            }
        }

        import_prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let cheap_threshold_price = if !import_prices.is_empty() {
            let idx = import_prices.len() / 5;
            import_prices[idx]
        } else {
            12.0
        };

        let night_average_price = if !night_prices.is_empty() {
            night_prices.iter().sum::<f64>() / night_prices.len() as f64
        } else {
            30.0
        };

        (
            expected_solar_kwh,
            demand_energy_needed_kwh,
            cheap_threshold_price,
            night_energy_needed_kwh,
            night_average_price,
        )
    }
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(s, "%k:%M:%S"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%I:%M:%S %p"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%k:%M"))
        .ok()
}

pub async fn run_power_manager_queue_task(
    config: SolaxBatteryControlConfig,
    mqtt_config: crate::config::MqttBrokerConfig,
    mut rx_telemetry: tokio::sync::mpsc::Receiver<crate::dispatch_manager::TelemetryBatch>,
    mut rx_command: tokio::sync::mpsc::Receiver<crate::dispatch_manager::DriverCommand>,
    tx_dispatch_agg: tokio::sync::mpsc::Sender<crate::dispatch_manager::TelemetryBatch>,
    cancel_token: CancellationToken,
    db_path: String,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let (mqtt_client, mut eventloop) = crate::mqtt_helper::create_mqtt_client("powerscraper-power-manager", &mqtt_config);

    let cancel_eventloop = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_eventloop.cancelled() => break,
                _ = eventloop.poll() => {}
            }
        }
    });

    let pm = Arc::new(Mutex::new(PowerManager::new(config.clone(), base_topic.clone())));

    let inverter_names: Vec<String> = config.inverter.keys().cloned().collect();
    update_all_inferred_capacities(&db_path, &inverter_names);

    let tm = {
        let pm_lock = pm.lock().await;
        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
            status.active_mode = pm_lock.mode.to_string();
            status.grid_target = pm_lock.grid_target;
            status.mqtt_enabled = crate::config::Config::load_from_db(&db_path).map(|c| c.mqtt.is_some()).unwrap_or(false);
            let aggregates = pm_lock.calculate_aggregates();
            status.usage = aggregates.get("Usage").copied();
            status.power_budget = aggregates.get("Power Budget").copied();
            status.power_budget_with_charging = aggregates.get("Power Budget with charging").copied();
            let rates = pm_lock.tariff_manager.get_current_rates();
            status.import_price = Some(rates.import_rate);
            status.export_price = Some(rates.export_rate);
        }
        pm_lock.tariff_manager.clone()
    };
    let cancel_token_clone = cancel_token.clone();
    tokio::spawn(async move {
        tm.start_background_loop(cancel_token_clone).await;
    });

    let cancel_token_weather = cancel_token.clone();
    let db_path_weather = db_path.clone();
    tokio::spawn(async move {
        run_weather_fetcher_task(db_path_weather, cancel_token_weather).await;
    });

    println!("Power Manager queue processor running...");

    let mut last_threshold_calc = std::time::Instant::now();
    let mut history_ticker = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                println!("Power Manager queue processor shutting down...");
                break;
            }
            _ = history_ticker.tick() => {
                if last_threshold_calc.elapsed() >= Duration::from_secs(24 * 3600) {
                    if let Ok(thresholds) = calculate_price_thresholds(&db_path) {
                        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                            status.price_thresholds = Some(thresholds);
                        }
                        last_threshold_calc = std::time::Instant::now();
                    }
                    let inv_names: Vec<String> = config.inverter.keys().cloned().collect();
                    update_all_inferred_capacities(&db_path, &inv_names);
                }
            }
            Some(cmd) = rx_command.recv() => {
                let mut pm_lock = pm.lock().await;
                match cmd {
                    crate::dispatch_manager::DriverCommand::SetMode { mode } => {
                        if let Ok(new_mode) = mode.parse::<PowerManagerMode>() {
                            pm_lock.mode = new_mode;
                            println!("Power Manager mode set via queue to: {}", new_mode);
                            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                status.active_mode = new_mode.to_string();
                            }
                            let inv_names: Vec<String> = pm_lock.config.inverter.keys().cloned().collect();
                            for inv in inv_names {
                                pm_lock.command_group(&inv, &mqtt_client, &base_topic).await;
                            }
                        }
                    }
                    crate::dispatch_manager::DriverCommand::SetGridTarget { watts } => {
                        pm_lock.grid_target = watts;
                        println!("Power Manager grid target set via queue to: {}", watts);
                        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                            status.grid_target = watts;
                        }
                        let inv_names: Vec<String> = pm_lock.config.inverter.keys().cloned().collect();
                        for inv in inv_names {
                            pm_lock.command_group(&inv, &mqtt_client, &base_topic).await;
                        }
                    }
                    _ => {}
                }
            }
            Some(batch) = rx_telemetry.recv() => {
                let mut pm_lock = pm.lock().await;
                let mut state_changed = false;

                let source_opt = pm_lock.config.source.clone();
                let is_source = source_opt.as_ref().map(|s| s == &batch.device_name).unwrap_or(false);

                for (metric, &val) in &batch.metrics {
                    if is_source {
                        if metric == "Total system power" {
                            pm_lock.total_power = val;
                            state_changed = true;
                            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                status.meter_power = val;
                                status.meter_last_updated = Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
                            }
                        } else if metric == "Phase 1 power" {
                            pm_lock.phase_power[1] = val;
                            state_changed = true;
                        } else if metric == "Phase 2 power" {
                            pm_lock.phase_power[2] = val;
                            state_changed = true;
                        } else if metric == "Phase 3 power" {
                            pm_lock.phase_power[3] = val;
                            state_changed = true;
                        }
                    }

                    if let Some(inv_cfg) = pm_lock.config.inverter.get(&batch.device_name) {
                        let mut state = pm_lock.inverters.get(&batch.device_name).cloned().unwrap_or_default();
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
                            if pm_lock.config.source.is_none() {
                                let phase = inv_cfg.phase;
                                pm_lock.handle_inverter_power(&batch.device_name, phase, val);
                            }
                        }

                        if updated {
                            pm_lock.inverters.insert(batch.device_name.clone(), state);
                            state_changed = true;
                        }
                    }
                }

                if state_changed {
                    let inv_names: Vec<String> = pm_lock.config.inverter.keys().cloned().collect();
                    for inv in inv_names {
                        pm_lock.command_group(&inv, &mqtt_client, &base_topic).await;
                    }

                    let aggregates = pm_lock.calculate_aggregates();
                    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                        status.usage = aggregates.get("Usage").copied();
                        status.power_budget = aggregates.get("Power Budget").copied();
                        status.power_budget_with_charging = aggregates.get("Power Budget with charging").copied();
                        status.active_mode = pm_lock.mode.to_string();
                        status.grid_target = pm_lock.grid_target;
                        status.mqtt_enabled = crate::config::Config::load_from_db(&db_path).map(|c| c.mqtt.is_some()).unwrap_or(false);
                        let rates = pm_lock.tariff_manager.get_current_rates();
                        status.import_price = Some(rates.import_rate);
                        status.export_price = Some(rates.export_rate);
                    }

                    let agg_batch = crate::dispatch_manager::TelemetryBatch {
                        device_name: "aggregate".to_string(),
                        timestamp: chrono::Utc::now().timestamp(),
                        metrics: aggregates,
                    };
                    let _ = tx_dispatch_agg.try_send(agg_batch);
                }
            }
        }
    }
}

pub async fn run_power_manager_task(
    config: SolaxBatteryControlConfig,
    mqtt_config: MqttBrokerConfig,
    cancel_token: CancellationToken,
    db_path: String,
) {
    let history_config = crate::config::Config::load_from_db(&db_path)
        .ok()
        .and_then(|c| c.history);
    let history_enabled = history_config.as_ref().map(|h| h.enabled).unwrap_or(true);
    let flush_interval_mins = history_config.as_ref().map(|h| h.flush_interval_mins).unwrap_or(30);
    let retention_days = history_config.as_ref().and_then(|h| h.retention_days);

    if history_enabled {
        if let Err(e) = init_history_db(&db_path) {
            eprintln!("Failed to initialize telemetry history table: {}", e);
        }
    }

    let mut latest_telemetry: HashMap<String, f64> = HashMap::new();
    let mut history_ticker = tokio::time::interval(Duration::from_secs(10));
    history_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_flush = std::time::Instant::now();
    let mut last_threshold_calc = std::time::Instant::now() - Duration::from_secs(25 * 3600);

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
    {
        pm.lock().await.db_path = db_path.clone();
    }

    // Calculate inferred capacities at startup
    let inverter_names: Vec<String> = config.inverter.keys().cloned().collect();
    update_all_inferred_capacities(&db_path, &inverter_names);

    // Spawn Amber tariff manager polling loop if configured
    let tm = {
        let pm_lock = pm.lock().await;
        pm_lock.tariff_manager.clone()
    };
    let cancel_token_clone = cancel_token.clone();
    tokio::spawn(async move {
        tm.start_background_loop(cancel_token_clone).await;
    });

    let cancel_token_weather = cancel_token.clone();
    let db_path_weather = db_path.clone();
    tokio::spawn(async move {
        run_weather_fetcher_task(db_path_weather, cancel_token_weather).await;
    });

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

    // Publish aggregates Home Assistant discovery configs
    let aggregate_metrics = [
        "Total Solar Production",
        "Total Grid Power Used for Charging",
        "Total Consumption",
        "Total Charging",
        "Total Discharging",
        "Power Budget",
        "Power Budget with Charging",
    ];
    for metric in &aggregate_metrics {
        crate::mqtt_helper::publish_home_assistant_discovery(
            &mqtt_client,
            &mqtt_config,
            "aggregate",
            metric,
            false,
        )
        .await;
    }

    // Publish tariff and Amber Home Assistant discovery configs
    let tariff_discovery = [
        ("tariff", "import_price"),
        ("tariff", "export_price"),
        ("amber", "import_price"),
        ("amber", "export_price"),
        ("amber", "general_price"),
        ("amber", "feedin_price"),
    ];
    for (device, metric) in &tariff_discovery {
        crate::mqtt_helper::publish_home_assistant_discovery(
            &mqtt_client,
            &mqtt_config,
            device,
            metric,
            false,
        )
        .await;
    }

    // Publish initial state and update global system status
    {
        let pm_lock = pm.lock().await;
        let rates = pm_lock.tariff_manager.get_current_rates();
        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
            status.active_mode = pm_lock.mode.to_string();
            status.grid_target = pm_lock.grid_target;
            status.import_price = Some(rates.import_rate);
            status.export_price = Some(rates.export_rate);
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

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<rumqttc::Event, ()>>();
    let cancel_token_clone = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => break,
                res = eventloop.poll() => {
                    match res {
                        Ok(notification) => {
                            if tx.send(Ok(notification)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("Power Manager MQTT eventloop error: {}", e);
                            if tx.send(Err(())).is_err() {
                                break;
                            }
                            tokio::select! {
                                _ = cancel_token_clone.cancelled() => break,
                                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                            }
                        }
                    }
                }
            }
        }
    });

    let mut currently_connected = false;
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = history_ticker.tick() => {
                if last_threshold_calc.elapsed() >= Duration::from_secs(24 * 3600) {
                    match calculate_price_thresholds(&db_path) {
                        Ok(thresholds) => {
                            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                status.price_thresholds = Some(thresholds);
                            }
                            last_threshold_calc = std::time::Instant::now();
                            println!("Calculated daily price thresholds: {:?}", thresholds);
                        }
                        Err(e) => {
                            eprintln!("Failed to calculate price thresholds: {}", e);
                        }
                    }
                    // Run daily battery capacity inference
                    let inverter_names: Vec<String> = config.inverter.keys().cloned().collect();
                    update_all_inferred_capacities(&db_path, &inverter_names);
                }

                if history_enabled {
                    let rates = {
                        let pm_lock = pm.lock().await;
                        pm_lock.tariff_manager.get_current_rates()
                    };
                    latest_telemetry.insert("tariff/import_price".to_string(), rates.import_rate);
                    latest_telemetry.insert("tariff/export_price".to_string(), rates.export_rate);
                    latest_telemetry.insert("amber/import_price".to_string(), rates.import_rate);
                    latest_telemetry.insert("amber/export_price".to_string(), rates.export_rate);
                    latest_telemetry.insert("amber/general_price".to_string(), rates.import_rate);
                    latest_telemetry.insert("amber/feedin_price".to_string(), rates.export_rate);

                    // Publish price metrics to MQTT
                    let _ = mqtt_client.publish(format!("{}/tariff/import_price", base_topic), QoS::AtLeastOnce, false, rates.import_rate.to_string()).await;
                    let _ = mqtt_client.publish(format!("{}/tariff/export_price", base_topic), QoS::AtLeastOnce, false, rates.export_rate.to_string()).await;
                    let _ = mqtt_client.publish(format!("{}/amber/import_price", base_topic), QoS::AtLeastOnce, false, rates.import_rate.to_string()).await;
                    let _ = mqtt_client.publish(format!("{}/amber/export_price", base_topic), QoS::AtLeastOnce, false, rates.export_rate.to_string()).await;
                    let _ = mqtt_client.publish(format!("{}/amber/general_price", base_topic), QoS::AtLeastOnce, false, rates.import_rate.to_string()).await;
                    let _ = mqtt_client.publish(format!("{}/amber/feedin_price", base_topic), QoS::AtLeastOnce, false, rates.export_rate.to_string()).await;

                    let now_ts = Utc::now().timestamp();
                    let new_recs: Vec<HistoryRecord> = latest_telemetry
                        .iter()
                        .map(|(topic, &value)| HistoryRecord {
                            timestamp: now_ts,
                            topic: topic.clone(),
                            value,
                        })
                        .collect();
                    crate::database::push_pending_history_records(new_recs);

                    if last_flush.elapsed() >= Duration::from_secs(flush_interval_mins as u64 * 60) {
                        crate::database::flush_pending_history_to_db(&db_path, retention_days);
                        last_flush = std::time::Instant::now();
                    }
                }
            }
            res = rx.recv() => {
                match res {
                    Some(Ok(notification)) => {
                        if !currently_connected {
                            currently_connected = true;
                            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                status.mqtt_connected = true;
                            }
                        }
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
                                                pm_lock.config.initial_mode = Some(new_mode.to_string());
                                                println!("Power Manager mode changed to: {}", new_mode);

                                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                    status.active_mode = new_mode.to_string();
                                                }

                                                // Save to SQLite DB
                                                if let Ok(mut db_cfg) = crate::config::Config::load_from_db(&db_path) {
                                                    if let Some(ref mut bat_ctrl) = db_cfg.battery_control {
                                                        bat_ctrl.initial_mode = Some(new_mode.to_string());
                                                        if let Err(e) = db_cfg.save_to_db(&db_path) {
                                                            eprintln!("Failed to save config to DB on mode change: {}", e);
                                                        }
                                                    }
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
                                                    pm_lock.command_group(&inv_name, &mqtt_client, &base_topic).await;
                                                }
                                            }
                                        } else if metric == "command/grid_target" {
                                            if let Ok(target) = payload_trim.parse::<f64>() {
                                                let mut pm_lock = pm.lock().await;
                                                pm_lock.grid_target = target;
                                                pm_lock.config.grid_target = Some(target);
                                                println!("Power Manager grid target changed to: {}", target);

                                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                    status.grid_target = target;
                                                }

                                                // Save to SQLite DB
                                                if let Ok(mut db_cfg) = crate::config::Config::load_from_db(&db_path) {
                                                    let bat_ctrl = db_cfg.battery_control.get_or_insert_with(Default::default);
                                                    bat_ctrl.grid_target = Some(target);
                                                    if let Err(e) = db_cfg.save_to_db(&db_path) {
                                                        eprintln!("Failed to save config to DB on target change: {}", e);
                                                    }
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
                                                    pm_lock.command_group(&inv_name, &mqtt_client, &base_topic).await;
                                                }
                                            }
                                        }
                                    } else if let Ok(val) = payload_trim.parse::<f64>() {
                                        if history_enabled {
                                            latest_telemetry.insert(format!("{}/{}", device_name, metric), val);
                                        }
                                        let mut pm_lock = pm.lock().await;
                                        let mut state_changed = false;

                                        // 1. Check if it's the configured power consumption source
                                        let source_opt = pm_lock.config.source.clone();
                                        let is_source = source_opt.as_ref().map(|s| s == device_name).unwrap_or(false);
                                        if is_source {
                                            if metric == "Total system power" {
                                                pm_lock.total_power = val;
                                                state_changed = true;
                                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                    status.meter_power = val;
                                                    status.meter_last_updated = Some(std::time::SystemTime::now()
                                                        .duration_since(std::time::UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_secs());
                                                    let rates = pm_lock.tariff_manager.get_current_rates();
                                                    status.import_price = Some(rates.import_rate);
                                                    status.export_price = Some(rates.export_rate);
                                                }
                                            } else if metric == "Phase 1 power" {
                                                pm_lock.phase_power[1] = val;
                                                state_changed = true;
                                            } else if metric == "Phase 2 power" {
                                                pm_lock.phase_power[2] = val;
                                                state_changed = true;
                                            } else if metric == "Phase 3 power" {
                                                pm_lock.phase_power[3] = val;
                                                state_changed = true;
                                            }
                                        }

                                        // 2. Check if it's an inverter telemetry update
                                        if let Some(inv_cfg) = pm_lock.config.inverter.get(device_name) {
                                            let mut state = pm_lock.inverters.get(device_name).cloned().unwrap_or_default();
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
                                                if pm_lock.config.source.is_none() {
                                                    let phase = inv_cfg.phase;
                                                    pm_lock.handle_inverter_power(device_name, phase, val);
                                                    if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                        status.meter_power = pm_lock.total_power;
                                                        status.meter_last_updated = Some(std::time::SystemTime::now()
                                                            .duration_since(std::time::UNIX_EPOCH)
                                                            .unwrap()
                                                            .as_secs());
                                                        let rates = pm_lock.tariff_manager.get_current_rates();
                                                        status.import_price = Some(rates.import_rate);
                                                        status.export_price = Some(rates.export_rate);
                                                    }
                                                }
                                            }

                                            if updated {
                                                pm_lock.inverters.insert(device_name.to_string(), state);
                                                state_changed = true;

                                                // Recalculate control and command via BatteryGroup
                                                pm_lock.command_group(device_name, &mqtt_client, &base_topic).await;
                                            }
                                        }

                                        if state_changed {
                                            let aggregates = pm_lock.calculate_aggregates();
                                            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                status.usage = aggregates.get("Usage").copied();
                                                status.power_budget = aggregates.get("Power Budget").copied();
                                                status.power_budget_with_charging = aggregates.get("Power Budget with charging").copied();
                                            }
                                            drop(pm_lock);

                                            for (metric_name, value) in aggregates {
                                                let topic = format!("{}/aggregate/{}", base_topic, metric_name);
                                                let _ = mqtt_client
                                                    .publish(&topic, QoS::AtLeastOnce, false, value.to_string())
                                                    .await;
                                            }
                                        } else {
                                            drop(pm_lock);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(())) => {
                        if currently_connected {
                            currently_connected = false;
                            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                status.mqtt_connected = false;
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    if history_enabled {
        println!("Shutting down Power Manager. Flushing remaining pending telemetry records to DB...");
        crate::database::flush_pending_history_to_db(&db_path, retention_days);
    }
}

#[derive(serde::Serialize, Clone, Default, Debug)]
pub struct DailyScenarioResult {
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub cycles: f64,
    pub energy_cost: f64,
}

#[derive(serde::Serialize, Clone, Default, Debug)]
pub struct SimulationResultModel {
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub cycles: f64,
    pub energy_cost: f64,
    pub demand_charges: f64,
    pub net_bill: f64,
    pub daily: std::collections::BTreeMap<String, DailyScenarioResult>,
    pub soc_history: Vec<f32>,
    pub grid_history: Vec<f32>,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct SimulationResponse {
    pub start_date: String,
    pub end_date: String,
    pub records_simulated: usize,
    pub total_solar_kwh: f64,
    pub total_usage_kwh: f64,
    pub no_battery: SimulationResultModel,
    pub baseline: SimulationResultModel,
    pub auto: SimulationResultModel,
    pub smart_heuristic: SimulationResultModel,
    pub lookahead_mpc: SimulationResultModel,
    pub adaptive_peak: SimulationResultModel,
    pub mpc_arbitrage: SimulationResultModel,
    pub evolved_heuristic: SimulationResultModel,
    pub suggest_charge_threshold: Option<f64>,
    pub suggest_discharge_threshold: Option<f64>,
    pub daily_solar: std::collections::BTreeMap<String, f64>,
    pub daily_usage: std::collections::BTreeMap<String, f64>,
    pub timestamps: Vec<i64>,
    pub solar_history: Vec<f32>,
    pub load_history: Vec<f32>,
    pub dates: Vec<String>,
    pub time_labels: Vec<String>,
}

pub fn run_historical_simulation(db_path: &str, range: &str) -> Result<SimulationResponse, String> {
    run_historical_simulation_impl(db_path, range, None)
}

pub fn load_sim_records(
    db_path: &str,
    range: &str,
) -> Result<(Vec<crate::simulation::SimRecord>, crate::simulation::SimConfig), String> {
    let config = crate::config::Config::load_from_db(db_path).unwrap_or_else(|_| crate::config::Config::default_empty());
    let evolved_heuristic_config = config.battery_control.as_ref()
        .and_then(|bc| bc.evolved_heuristic.clone())
        .unwrap_or_else(|| crate::config::EvolvedHeuristicConfig::default());
    let demand_window = get_demand_window(config.battery_control.as_ref());
    let demand_rate = config.battery_control.as_ref()
        .and_then(|bc| bc.demand.as_ref())
        .map(|d| d.rate)
        .unwrap_or(0.0);
    let mains_source = config.battery_control.as_ref()
        .and_then(|bc| bc.source.as_deref())
        .unwrap_or("MainsMeter");

    let (negative_export_prevent, low_price_charge, low_price_threshold, high_price_discharge, high_price_threshold) =
        if let Some(ref bc) = config.battery_control {
            if let Some(crate::config::TariffConfig::Amber {
                negative_export_prevent,
                low_price_charge,
                low_price_threshold,
                high_price_discharge,
                high_price_threshold,
                ..
            }) = bc.tariff.as_ref() {
                (*negative_export_prevent, *low_price_charge, *low_price_threshold, *high_price_discharge, *high_price_threshold)
            } else {
                (true, true, 15.0, true, 30.0) // default simulation settings for non-Amber configurations
            }
        } else {
            (true, true, 15.0, true, 30.0)
        };

    let mut battery_capacity_kwh = 0.0;
    let mut max_power_w = 0.0;
    let mut min_charge_pct = 20;
    let mut max_charge_pct = 100;

    if let Some(ref bc) = config.battery_control {
        std::thread::scope(|s| {
            let mut threads = Vec::new();
            for (inv_name, inv_cfg) in &bc.inverter {
                if !inv_cfg.has_battery() {
                    continue;
                }
                if let Some(cap) = inv_cfg.battery_capacity {
                    battery_capacity_kwh += cap;
                    max_power_w += inv_cfg.max_discharge.max(inv_cfg.max_charge);
                    if let Some(min_pct) = inv_cfg.min_charge_pct {
                        min_charge_pct = min_pct;
                    }
                    if let Some(max_pct) = inv_cfg.max_charge_pct {
                        max_charge_pct = max_pct;
                    }
                } else {
                    let inv_name_clone = inv_name.clone();
                    let inv_cfg_clone = inv_cfg.clone();
                    let handle = s.spawn(move || {
                        let cap = calculate_inferred_battery_capacity(db_path, &inv_name_clone).unwrap_or(13.8);
                        (cap, inv_cfg_clone)
                    });
                    threads.push(handle);
                }
            }

            for handle in threads {
                if let Ok((cap, inv_cfg)) = handle.join() {
                    battery_capacity_kwh += cap;
                    max_power_w += inv_cfg.max_discharge.max(inv_cfg.max_charge);
                    if let Some(min_pct) = inv_cfg.min_charge_pct {
                        min_charge_pct = min_pct;
                    }
                    if let Some(max_pct) = inv_cfg.max_charge_pct {
                        max_charge_pct = max_pct;
                    }
                }
            }
        });
    }

    if battery_capacity_kwh == 0.0 {
        battery_capacity_kwh = 13.8;
    }
    if max_power_w == 0.0 {
        max_power_w = 5000.0;
    }

    let now_ts = chrono::Local::now().timestamp();
    let start_ts = match range {
        "1d" => now_ts - 86400,
        "1w" => now_ts - 7 * 86400,
        "1m" => now_ts - 30 * 86400,
        "1y" => now_ts - 365 * 86400,
        _ => 0,
    };

    let rows = crate::database::get_all_telemetry_since(db_path, start_ts).map_err(|e| e.to_string())?;

    use std::collections::BTreeMap;
    use std::collections::HashMap;

    let mut groups: BTreeMap<i64, HashMap<String, f64>> = BTreeMap::new();
    for (ts, topic, val) in rows {
        let ts_rounded = (ts / 60) * 60;
        let entry = groups.entry(ts_rounded).or_insert_with(HashMap::new);
        entry.insert(topic, val);
    }

    let keys: Vec<i64> = groups.keys().cloned().collect();
    if keys.len() < 2 {
        return Err("Insufficient historical telemetry data in database to run simulation.".to_string());
    }

    let tz_offset = get_timezone_offset(config.battery_control.as_ref().and_then(|bc| bc.timezone.as_deref()));

    let mut records = Vec::new();
    for i in 0..keys.len() - 1 {
        let ts = keys[i];
        let next_ts = keys[i + 1];
        let duration_hours = (next_ts - ts) as f64 / 3600.0;
        if duration_hours > 2.0 {
            continue;
        }

        let g = &groups[&ts];
        
        let mut solar = 0.0;
        let mut battery = 0.0;
        let mut load = None;
        let mut import_price = None;
        let mut export_price = None;

        for (topic, val) in g {
            if topic.ends_with("/PV1 Power") || topic.ends_with("/PV2 Power") || topic.contains("/Input 1 Power") || topic.contains("/Input 2 Power") {
                solar += val;
            } else if topic == &format!("{}/Total system power", mains_source) {
                load = Some(*val);
            } else if topic.ends_with("/Battery Power") {
                battery += val;
            } else if topic == "tariff/import_price" {
                import_price = Some(*val);
            } else if topic == "tariff/export_price" {
                export_price = Some(*val);
            }
        }

        if load.is_none() {
            continue;
        }

        let dt_local = chrono::Utc.timestamp_opt(ts, 0)
            .single()
            .map(|utc| utc.with_timezone(&tz_offset))
            .unwrap_or_else(|| chrono::Utc::now().with_timezone(&tz_offset));

        let grid_w = load.unwrap();
        let gross_load_w = (grid_w + solar + battery).max(0.0);

        records.push(crate::simulation::SimRecord {
            timestamp: ts,
            dt_local,
            solar_power_w: solar,
            load_power_w: gross_load_w,
            import_price_cents: import_price.unwrap_or(f64::NAN),
            export_price_cents: export_price.unwrap_or(f64::NAN),
            duration_hours,
            day_solar_kwh: 0.0,
        });
    }

    if records.is_empty() {
        return Err("No aligned telemetry records found for simulation in range.".to_string());
    }

    // Precompute total daily solar generation (kWh)
    let mut daily_solar_kwh_map: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for r in &records {
        let date_str = r.dt_local.format("%Y-%m-%d").to_string();
        let kwh = (r.solar_power_w / 1000.0) * r.duration_hours;
        *daily_solar_kwh_map.entry(date_str).or_insert(0.0) += kwh;
    }

    for r in &mut records {
        let date_str = r.dt_local.format("%Y-%m-%d").to_string();
        r.day_solar_kwh = *daily_solar_kwh_map.get(&date_str).unwrap_or(&0.0);
    }

    let mut last_imp = 25.0;
    let mut last_exp = 8.0;

    for r in &mut records {
        if !r.import_price_cents.is_nan() {
            last_imp = r.import_price_cents;
        }
        r.import_price_cents = last_imp;

        if !r.export_price_cents.is_nan() {
            last_exp = r.export_price_cents;
        }
        r.export_price_cents = last_exp;
    }

    let periods_list = if let Some(ref bc) = config.battery_control {
        bc.period.values().cloned().collect()
    } else {
        Vec::new()
    };

    let sim_config = crate::simulation::SimConfig {
        battery_capacity_kwh,
        max_power_w,
        min_charge_pct: min_charge_pct as u8,
        max_charge_pct: max_charge_pct as u8,
        demand_window,
        demand_rate,
        negative_export_prevent,
        low_price_charge,
        low_price_threshold,
        high_price_discharge,
        high_price_threshold,
        periods: periods_list,
        evolved_heuristic: evolved_heuristic_config,
        evolved_heuristic_monthly: config.battery_control.as_ref().and_then(|bc| bc.evolved_heuristic_monthly.clone()),
        min_charge_hysteresis: config.battery_control.as_ref().and_then(|bc| bc.min_charge_hysteresis),
    };

    Ok((records, sim_config))
}

pub fn run_historical_simulation_impl(
    db_path: &str,
    range: &str,
    progress_cb: Option<&(dyn Fn(f64, f64) + Send + Sync)>,
) -> Result<SimulationResponse, String> {
    let (records, sim_config) = load_sim_records(db_path, range)?;

    let start_date = records.first().unwrap().dt_local.format("%Y-%m-%d %H:%M:%S").to_string();
    let end_date = records.last().unwrap().dt_local.format("%Y-%m-%d %H:%M:%S").to_string();
    let records_simulated = records.len();

    let sim_config_ref = &sim_config;
    let records_ref = &records;

    let threads_res =
        std::thread::scope(|s| {
            let t_no_bat = s.spawn(move || {
                crate::simulation::no_battery::run(records_ref, sim_config_ref)
            });

            let t_base = s.spawn(move || {
                crate::simulation::baseline::run(records_ref, sim_config_ref)
            });

            let t_auto = s.spawn(move || {
                crate::simulation::auto::run(records_ref, sim_config_ref)
            });

            let t_smart = s.spawn(move || {
                crate::simulation::smart_heuristic::run(records_ref, sim_config_ref)
            });

            let t_mpc = s.spawn(move || {
                crate::simulation::lookahead_mpc::run(records_ref, sim_config_ref, progress_cb)
            });

            let t_adapt = s.spawn(move || {
                crate::simulation::adaptive_peak::run(records_ref, sim_config_ref)
            });

            let t_arb = s.spawn(move || {
                crate::simulation::mpc_arbitrage::run(records_ref, sim_config_ref)
            });

            let t_evolved = s.spawn(move || {
                crate::simulation::evolved_heuristic::run(records_ref, sim_config_ref)
            });

            (
                t_no_bat.join().unwrap(),
                t_base.join().unwrap(),
                t_auto.join().unwrap(),
                t_smart.join().unwrap(),
                t_mpc.join().unwrap(),
                t_adapt.join().unwrap(),
                t_arb.join().unwrap(),
                t_evolved.join().unwrap(),
            )
        });

    let mut daily_solar = std::collections::BTreeMap::new();
    let mut daily_usage = std::collections::BTreeMap::new();
    let mut total_solar_kwh = 0.0;
    let mut total_usage_kwh = 0.0;
    for r in &records {
        let sol = (r.solar_power_w / 1000.0) * r.duration_hours;
        let usg = (r.load_power_w / 1000.0) * r.duration_hours;
        total_solar_kwh += sol;
        total_usage_kwh += usg;
        
        let date_str = r.dt_local.date_naive().to_string();
        *daily_solar.entry(date_str.clone()).or_insert(0.0) += sol;
        *daily_usage.entry(date_str).or_insert(0.0) += usg;
    }

    let mut import_prices: Vec<f64> = records.iter().map(|r| r.import_price_cents).collect();
    let mut export_prices: Vec<f64> = records.iter().map(|r| r.export_price_cents).collect();

    import_prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    export_prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let (suggest_charge_threshold, suggest_discharge_threshold) = if !import_prices.is_empty() && !export_prices.is_empty() {
        let median_import = import_prices[import_prices.len() / 2];
        let median_export = export_prices[export_prices.len() / 2];

        // 1st percentile of import, capped at median_export
        let idx_charge = ((import_prices.len() - 1) as f64 * 0.01) as usize;
        let p1_import = import_prices[idx_charge];
        let charge_val = p1_import.min(median_export);
        let suggest_charge = (charge_val * 10.0).round() / 10.0;

        // 99th percentile of export, floored at 3x median_import
        let idx_discharge = ((export_prices.len() - 1) as f64 * 0.99) as usize;
        let p99_export = export_prices[idx_discharge];
        let discharge_val = p99_export.max(median_import * 3.0);
        let suggest_discharge = (discharge_val * 10.0).round() / 10.0;

        // Ensure charge < discharge
        let mut final_charge = suggest_charge;
        if final_charge >= suggest_discharge {
            final_charge = suggest_discharge - 1.0;
        }

        (Some(final_charge), Some(suggest_discharge))
    } else {
        (None, None)
    };

    let (no_battery_res, baseline_res, auto_res, smart_heuristic_res, lookahead_mpc_res, adaptive_peak_res, mpc_arbitrage_res, evolved_res) = threads_res;

    let timestamps = records.iter().map(|r| r.timestamp).collect();
    let solar_history = records.iter().map(|r| r.solar_power_w as f32).collect();
    let load_history = records.iter().map(|r| r.load_power_w as f32).collect();
    let dates = records.iter().map(|r| r.dt_local.date_naive().to_string()).collect();
    let time_labels = records.iter().map(|r| r.dt_local.format("%H:%M").to_string()).collect();

    Ok(SimulationResponse {
        start_date,
        end_date,
        records_simulated,
        total_solar_kwh,
        total_usage_kwh,
        no_battery: no_battery_res,
        baseline: baseline_res,
        auto: auto_res,
        smart_heuristic: smart_heuristic_res,
        lookahead_mpc: lookahead_mpc_res,
        adaptive_peak: adaptive_peak_res,
        mpc_arbitrage: mpc_arbitrage_res,
        evolved_heuristic: evolved_res,
        suggest_charge_threshold,
        suggest_discharge_threshold,
        daily_solar,
        daily_usage,
        timestamps,
        solar_history,
        load_history,
        dates,
        time_labels,
    })
}

fn get_demand_window(config: Option<&SolaxBatteryControlConfig>) -> Option<(NaiveTime, NaiveTime)> {
    let bc = config?;
    let demand = bc.demand.as_ref()?;
    let start = parse_time(&demand.start)?;
    let end = parse_time(&demand.end)?;
    Some((start, end))
}

fn is_time_in_window(now_time: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start < end {
        now_time >= start && now_time < end
    } else {
        !(now_time >= end && now_time < start)
    }
}

// --- Weather & Solar Forecast Logic ---

pub fn calculate_solar_position(lat: f64, lon: f64, utc_time: chrono::DateTime<chrono::Utc>) -> (f64, f64) {
    use chrono::Datelike;
    use chrono::Timelike;
    let lat_rad = lat.to_radians();
    let d = utc_time.ordinal() as f64;
    
    // Declination angle delta (radians)
    let delta = (23.45_f64.to_radians()) * ((2.0 * std::f64::consts::PI * (284.0 + d) / 365.0).sin());
    
    // Equation of Time (EoT) in minutes
    let b = (360.0 * (d - 81.0) / 364.0).to_radians();
    let eot = 9.87 * (2.0 * b).sin() - 7.53 * b.cos() - 1.5 * b.sin();
    
    // Hour of day in UTC
    let utc_hour = utc_time.hour() as f64 + utc_time.minute() as f64 / 60.0 + utc_time.second() as f64 / 3600.0;
    
    // Solar Time in hours (lon / 15.0 converts longitude to hours timezone offset)
    let solar_time = utc_hour + lon / 15.0 + eot / 60.0;
    
    // Hour Angle H (radians)
    let h = (15.0 * (solar_time - 12.0)).to_radians();
    
    // Solar Altitude (elevation) alpha (radians)
    let sin_alpha = lat_rad.sin() * delta.sin() + lat_rad.cos() * delta.cos() * h.cos();
    let alpha = sin_alpha.asin();
    
    // Solar Azimuth theta_s (radians)
    let cos_alpha = alpha.cos();
    let cos_theta_s = if cos_alpha.abs() > 1e-6 {
        ((delta.sin() * lat_rad.cos() - delta.cos() * lat_rad.sin() * h.cos()) / cos_alpha).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let sin_theta_s = if cos_alpha.abs() > 1e-6 {
        (-delta.cos() * h.sin() / cos_alpha).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let theta_s = sin_theta_s.atan2(cos_theta_s);
    
    (alpha, theta_s)
}

pub fn calculate_poa_irradiance(
    dni: f64,
    dhi: f64,
    solar_elevation: f64,
    solar_azimuth: f64,
    tilt_deg: f64,
    azimuth_deg: f64,
) -> f64 {
    if solar_elevation <= 0.0 {
        return 0.0;
    }
    
    let tilt_rad = tilt_deg.to_radians();
    let azimuth_rad = azimuth_deg.to_radians();
    
    let cos_incidence = solar_elevation.sin() * tilt_rad.cos()
        + solar_elevation.cos() * tilt_rad.sin() * (solar_azimuth - azimuth_rad).cos();
    
    let direct_poa = dni * cos_incidence.max(0.0);
    let diffuse_poa = dhi * (1.0 + tilt_rad.cos()) / 2.0;
    
    direct_poa + diffuse_poa
}

pub async fn run_weather_fetcher_task(db_path: String, cancel_token: tokio_util::sync::CancellationToken) {
    println!("Spawning Weather Forecast Task...");
    let client = reqwest::Client::new();
    
    // Sleep 5 seconds initially to let startup settle
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    
    loop {
        if cancel_token.is_cancelled() {
            break;
        }
        
        let config = crate::config::Config::load_from_db(&db_path).ok();
        let location_opt = config.and_then(|c| c.location);
        
        if let Some(loc) = location_opt {
            let lat = loc.latitude;
            let lon = loc.longitude;
            
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&hourly=direct_normal_irradiance,diffuse_radiation&forecast_days=2&timezone=UTC",
                lat, lon
            );
            
            println!("Fetching weather forecast from Open-Meteo for lat: {}, lon: {}...", lat, lon);
            match client.get(&url).send().await {
                Ok(resp) => {
                    #[derive(serde::Deserialize, Debug)]
                    struct OpenMeteoHourly {
                        time: Vec<String>,
                        direct_normal_irradiance: Vec<f64>,
                        diffuse_radiation: Vec<f64>,
                    }
                    #[derive(serde::Deserialize, Debug)]
                    struct OpenMeteoResponse {
                        hourly: OpenMeteoHourly,
                    }
                    
                    match resp.json::<OpenMeteoResponse>().await {
                        Ok(data) => {
                            println!("Successfully received weather forecast data ({} slots)", data.hourly.time.len());
                            let mut predictions = Vec::new();
                            for i in 0..data.hourly.time.len() {
                                let t_str = &data.hourly.time[i];
                                let rfc_str = format!("{}:00Z", t_str);
                                if let Ok(utc_dt) = chrono::DateTime::parse_from_rfc3339(&rfc_str) {
                                    let utc_dt = utc_dt.with_timezone(&chrono::Utc);
                                    let (el, az) = calculate_solar_position(lat, lon, utc_dt);
                                    
                                    let mut hourly_w = 0.0;
                                    let dni = data.hourly.direct_normal_irradiance[i];
                                    let dhi = data.hourly.diffuse_radiation[i];
                                    
                                    for array in &loc.arrays {
                                        let poa = calculate_poa_irradiance(dni, dhi, el, az, array.tilt, array.azimuth);
                                        hourly_w += array.capacity_w() * (poa / 1000.0) * 0.85;
                                    }
                                    
                                    predictions.push((utc_dt.timestamp(), hourly_w));
                                }
                            }
                            
                            if !predictions.is_empty() {
                                if let Err(e) = crate::database::delete_and_save_solar_forecast(&db_path, &predictions) {
                                    eprintln!("Failed to save solar forecast: {}", e);
                                } else {
                                    println!("Saved {} hourly solar predictions to solar_forecast table.", predictions.len());
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to parse weather forecast JSON: {}", e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to fetch weather forecast from Open-Meteo: {}", e);
                }
            }
        }
        
        for _ in 0..720 {
            if cancel_token.is_cancelled() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::flush_history_to_db;
    use chrono::Local;

    #[test]
    fn test_hysteresis_state_transitions() {
        let config = crate::config::SolaxBatteryControlConfig {
            source: None,
            linked_batteries: true,
            timezone: None,
            inverter: HashMap::new(),
            period: HashMap::new(),
            grid_target: None,
            initial_mode: None,
            tariff: None,
            demand: None,
            evolved_heuristic: None,
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: Some(5), // 5% global hysteresis
        };

        let mut pm = PowerManager::new(config, "powerscraper".to_string());
        
        let period = BatteryControlPeriod {
            start: "00:00:00".to_string(),
            end: "23:59:59".to_string(),
            min_charge: 20, // Target is 20%
            grid_charge: true,
            force_discharge: None,
            grace: false,
            prefer_battery: false,
            min_charge_hysteresis: None, // use global
        };

        // Battery 1 at 21% (not low, was not low)
        let bat1 = crate::battery_group::Battery {
            name: "bat1".to_string(),
            capacity_wh: 10000.0,
            current_soc_pct: 21.0,
            min_soc_pct: 20.0,
            max_soc_pct: 95.0,
            max_charge_power_w: 3000.0,
            max_discharge_power_w: 3000.0,
        };

        // 1. Initial check at 21% SOC -> should not trigger low capacity
        let any_low = pm.check_low_capacity(&[bat1.clone()], &period);
        assert!(!any_low);
        assert_eq!(pm.low_capacity_state.get("bat1"), Some(&false));

        // 2. SOC drops to 19% (below min_charge 20%) -> should trigger low capacity
        let mut bat2 = bat1.clone();
        bat2.current_soc_pct = 19.0;
        let any_low = pm.check_low_capacity(&[bat2.clone()], &period);
        assert!(any_low);
        assert_eq!(pm.low_capacity_state.get("bat1"), Some(&true));

        // 3. SOC rises to 21% (above min_charge but within hysteresis threshold 20% + 5% = 25%)
        // Since it was already low, it should remain low!
        let mut bat3 = bat1.clone();
        bat3.current_soc_pct = 21.0;
        let any_low = pm.check_low_capacity(&[bat3.clone()], &period);
        assert!(any_low);
        assert_eq!(pm.low_capacity_state.get("bat1"), Some(&true));

        // 4. SOC rises to 26% (above 25% threshold) -> should release low capacity
        let mut bat4 = bat1.clone();
        bat4.current_soc_pct = 26.0;
        let any_low = pm.check_low_capacity(&[bat4.clone()], &period);
        assert!(!any_low);
        assert_eq!(pm.low_capacity_state.get("bat1"), Some(&false));

        // 5. Test period-specific hysteresis override (override global 5% with period-specific 10%)
        let mut period_with_hyst = period.clone();
        period_with_hyst.min_charge_hysteresis = Some(10);

        // Drops to 19% again to trigger low
        let mut bat5 = bat1.clone();
        bat5.current_soc_pct = 19.0;
        let any_low = pm.check_low_capacity(&[bat5.clone()], &period_with_hyst);
        assert!(any_low);
        assert_eq!(pm.low_capacity_state.get("bat1"), Some(&true));

        // Rises to 28% (above 20 + 5 global, but within 20 + 10 = 30% period specific hysteresis)
        // Since period specific is 10%, it should still remain low!
        let mut bat6 = bat1.clone();
        bat6.current_soc_pct = 28.0;
        let any_low = pm.check_low_capacity(&[bat6.clone()], &period_with_hyst);
        assert!(any_low);
        assert_eq!(pm.low_capacity_state.get("bat1"), Some(&true));

        // Rises to 31% (above 30% threshold) -> should release
        let mut bat7 = bat1.clone();
        bat7.current_soc_pct = 31.0;
        let any_low = pm.check_low_capacity(&[bat7.clone()], &period_with_hyst);
        assert!(!any_low);
        assert_eq!(pm.low_capacity_state.get("bat1"), Some(&false));
    }

    #[test]
    fn test_print_simulation() {
        let db_to_test = if std::path::Path::new("config.db").exists() {
            "config.db"
        } else if std::path::Path::new("remote_config.db").exists() {
            "remote_config.db"
        } else {
            return;
        };
        println!("TESTING SIMULATION ON DATABASE: {}", db_to_test);
        // Verify that the table 'telemetry_history' exists in the database before proceeding
        let conn = match crate::database::open_db_conn(db_to_test) {
            Ok(c) => c,
            Err(_) => return,
        };
        let table_exists: Result<String, _> = conn.query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='telemetry_history'",
            [],
            |row| row.get(0),
        );
        if table_exists.is_err() {
            return;
        }
        let config = crate::config::Config::load_from_db(db_to_test).unwrap();
        let demand_window = get_demand_window(config.battery_control.as_ref());
        let demand_rate = config.battery_control.as_ref()
            .and_then(|bc| bc.demand.as_ref())
            .map(|d| d.rate)
            .unwrap_or(0.0);
        println!("TEST CONFIG: demand_window={:?}, demand_rate={}", demand_window, demand_rate);

        // Let's connect directly to see the timestamps and dt_local conversion
        let tz_offset = get_timezone_offset(config.battery_control.as_ref().and_then(|bc| bc.timezone.as_deref()));
        let rows = crate::database::get_all_telemetry_since(db_to_test, 0).unwrap();
        let mut count = 0;
        let mut in_window_count = 0;
        for (ts, topic, _val) in rows {
            if topic == "MainsMeter/Total system power" {
                let dt_local = chrono::Utc.timestamp_opt(ts, 0)
                    .single()
                    .map(|utc| utc.with_timezone(&tz_offset))
                    .unwrap_or_else(|| chrono::Utc::now().with_timezone(&tz_offset));
                let now_time = dt_local.time();
                if count < 5 {
                    println!("FIRST RECORD: ts={}, dt_local={}, now_time={:?}", ts, dt_local, now_time);
                }
                if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                    in_window_count += 1;
                    if in_window_count < 5 {
                        println!("IN WINDOW RECORD: ts={}, dt_local={}, now_time={:?}", ts, dt_local, now_time);
                    }
                }
                count += 1;
            }
        }
        println!("TOTAL MAINS METER RECORDS: {}, IN WINDOW RECORDS: {}", count, in_window_count);

        println!("RUNNING HISTORICAL SIMULATION FOR 1m ON {}...", db_to_test);
        match run_historical_simulation_impl(db_to_test, "1m", None) {
            Ok(res) => {
                println!("RESULT_START_DATE: {}", res.start_date);
                println!("RESULT_END_DATE: {}", res.end_date);
                println!("RESULT_RECORDS: {}", res.records_simulated);
                println!("TOTAL SOLAR: {:.2} kWh, TOTAL USAGE: {:.2} kWh", res.total_solar_kwh, res.total_usage_kwh);
                
                let print_model = |name: &str, m: &SimulationResultModel| {
                    println!(
                        "{:<20} | Bill: ${:<8.2} | Cycles: {:<6.2} | Energy: ${:<8.2} | Demand: ${:<8.2}",
                        name, m.net_bill, m.cycles, m.energy_cost, m.demand_charges
                    );
                };
                
                println!("{:=<90}", "");
                print_model("No Battery", &res.no_battery);
                print_model("Baseline", &res.baseline);
                print_model("Auto", &res.auto);
                print_model("Smart Heuristic", &res.smart_heuristic);
                print_model("Lookahead MPC", &res.lookahead_mpc);
                print_model("Adaptive Peak", &res.adaptive_peak);
                print_model("MPC Arbitrage", &res.mpc_arbitrage);
                print_model("Evolved Heuristic", &res.evolved_heuristic);
                println!("{:=<90}", "");
            }
            Err(e) => {
                println!("ERROR: {}", e);
            }
        }
    }

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
            min_charge_hysteresis: None,
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
            battery_capacity: None,
            max_charge_pct: None,
            min_charge_pct: None,
            no_pv: None,
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
        inverters.insert("solax2".to_string(), mock_inverter(2, 2000.0, 2000.0));

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

        let state1 = InverterState {
            battery_capacity: 50,
            ..Default::default()
        };
        let state2 = InverterState {
            battery_capacity: 50,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state1);
        pm.inverters.insert("solax2".to_string(), state2);

        pm.assist_needed.insert("solax1".to_string(), true);
        pm.assist_needed.insert("solax2".to_string(), true);

        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert!(cmd1.is_some());
        assert!(cmd2.is_some());
    }

    #[test]
    fn test_linked_batteries_coordination_and_non_opposition() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 2000.0));
        inverters.insert("solax2".to_string(), mock_inverter(2, 2000.0, 2000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: true,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        // Scenario 1: Grid Charge (forced)
        // One battery has capacity < min_charge (8 < 10) and period.grid_charge = true.
        // Both batteries should charge at max_charge (-max_charge -> 2000 command).
        pm.config.period.get_mut("Always").unwrap().grid_charge = true;
        pm.inverters.insert(
            "solax1".to_string(),
            InverterState {
                battery_capacity: 8,
                ..Default::default()
            },
        );
        pm.inverters.insert(
            "solax2".to_string(),
            InverterState {
                battery_capacity: 50,
                ..Default::default()
            },
        );

        let cmd1 = pm.evaluate_and_command("solax1").unwrap();
        let cmd2 = pm.evaluate_and_command("solax2").unwrap();
        // Since both should charge, commands should be positive (charge_battery commands)
        assert_eq!(cmd1, 2000);
        assert_eq!(cmd2, 2000);

        // Scenario 2: Prefer Battery
        // One battery has capacity < min_charge (8 < 10) and period.prefer_battery = true.
        // They should charge from pooled PV.
        // solax1 has PV = 1000W, solax2 has PV = 2000W. Total PV = 3000W.
        // Distributed charge rate = 3000 / 2 = 1500W.
        // Both commands should be 1500.
        let always_period = pm.config.period.get_mut("Always").unwrap();
        always_period.grid_charge = false;
        always_period.prefer_battery = true;

        pm.inverters.insert(
            "solax1".to_string(),
            InverterState {
                battery_capacity: 8,
                pv1_power: 500.0,
                pv2_power: 500.0, // total 1000W
                ..Default::default()
            },
        );
        pm.inverters.insert(
            "solax2".to_string(),
            InverterState {
                battery_capacity: 50,
                pv1_power: 1000.0,
                pv2_power: 1000.0, // total 2000W
                ..Default::default()
            },
        );

        let cmd1 = pm.evaluate_and_command("solax1").unwrap();
        let cmd2 = pm.evaluate_and_command("solax2").unwrap();
        assert_eq!(cmd1, 1977);
        assert_eq!(cmd2, 1022);

        // Scenario 3: Normal Regulation (discharging)
        // One battery has capacity < min_charge (8 < 10) but period.grid_charge = false, prefer_battery = false.
        // Total power is 1000W (needs to discharge).
        // solax1 is below min_charge, so it stays idle (0).
        // solax2 is above min_charge, so it discharges (negative command).
        // They should never oppose (no one should charge while other discharges).
        let always_period = pm.config.period.get_mut("Always").unwrap();
        always_period.prefer_battery = false;

        pm.inverters.insert(
            "solax1".to_string(),
            InverterState {
                battery_capacity: 8,
                ..Default::default()
            },
        );
        pm.inverters.insert(
            "solax2".to_string(),
            InverterState {
                battery_capacity: 50,
                ..Default::default()
            },
        );
        pm.total_power = 2000.0;
        pm.total_discharge_power = 1000.0; // pre-populated total discharge power

        let cmd1 = pm.evaluate_and_command("solax1").unwrap();
        let cmd2 = pm.evaluate_and_command("solax2").unwrap();
        assert_eq!(cmd1, 0); // clamped to 0 because empty
        assert!(cmd2 < 0); // discharging (negative command)

        // Scenario 4: Normal Regulation (charging)
        // solax1 has capacity = 50, solax2 has capacity = 100 (full).
        // Total power is -2000W (needs to charge).
        // solax2 is full, so it clamps to 0.
        // solax1 is normal, so it charges (positive command).
        // No opposition.
        pm.inverters.insert(
            "solax1".to_string(),
            InverterState {
                battery_capacity: 50,
                ..Default::default()
            },
        );
        pm.inverters.insert(
            "solax2".to_string(),
            InverterState {
                battery_capacity: 100,
                ..Default::default()
            },
        );
        pm.total_power = -2000.0;
        pm.total_discharge_power = -1000.0; // pre-populated

        let cmd1 = pm.evaluate_and_command("solax1").unwrap();
        let cmd2 = pm.evaluate_and_command("solax2").unwrap();
        assert!(cmd1 > 0); // charging (positive command)
        assert_eq!(cmd2, 0); // clamped to 0 because full
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
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let _ = pm.evaluate_and_command("solax1");
        assert_eq!(pm.total_discharge_power, 500.0);

        // High negative power to trigger clamp to -max_total_charge_power
        pm.total_power = -10000.0;
        pm.phase_power[1] = -10000.0; // Ensure negative phase power so assist_needed is not cleared
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
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
            tariff: None,
            demand: None,
            ..Default::default()
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
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
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
            "charge batteries".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::ChargeBatteries)
        );
        assert_eq!(
            "charge-batteries".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::ChargeBatteries)
        );
        assert_eq!(
            "MAX_FEEDIN".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::MaximumFeedin)
        );
        assert_eq!(
            "maximum feedin".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::MaximumFeedin)
        );
        assert_eq!(
            "maximum-feedin".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::MaximumFeedin)
        );
        assert_eq!(
            "smart heuristic".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::SmartHeuristic)
        );
        assert_eq!(
            "adaptive peak shaving".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::AdaptivePeakShaving)
        );
        assert_eq!(
            "mpc optimizer".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::MpcOptimizer)
        );
        assert_eq!(
            "mpc arbitrage".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::MpcArbitrage)
        );
        assert_eq!(
            "arbitrage".parse::<PowerManagerMode>(),
            Ok(PowerManagerMode::MpcArbitrage)
        );
        assert_eq!("invalid".parse::<PowerManagerMode>(), Err(()));
    }

    #[test]
    fn test_evaluate_and_command_mode_transitions() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 3000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: true,
            timezone: None,
            inverter: inverters,
            period: periods,
            grid_target: Some(0.0),
            initial_mode: Some("Auto".to_string()),
            tariff: None,
            demand: None,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        let state = InverterState {
            battery_capacity: 50,
            discharge_power: 0.0,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        // 1. Initially Auto mode (linked): grid power is 400W import, target is 0W.
        // error = 400 - 0 = 400.
        // total_discharge_power += 400 * 0.1 = 40W.
        // returns -40.
        pm.total_power = 400.0;
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-40));

        // 2. Transition to ChargeBatteries mode: should force charging at max_charge (2000)
        pm.mode = PowerManagerMode::ChargeBatteries;
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(2000));

        // 3. Transition to MaximumFeedin mode: should force discharging at max_discharge (3000) -> negated is -3000
        pm.mode = PowerManagerMode::MaximumFeedin;
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-3000));

        // 4. Transition back to Auto: should continue Auto regulation from last total_discharge_power (40W)
        pm.mode = PowerManagerMode::Auto;
        // set grid power to -200W (feed in 200W).
        // error = -200 - 0 = -200.
        // total_discharge_power += -200 * 0.1 = -20W -> 40 - 20 = 20W.
        // returns -20.
        pm.total_power = -200.0;
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-20));
    }

    #[test]
    fn test_evaluate_and_command_mode_transitions_multi_inverter() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 3000.0));
        inverters.insert("solax2".to_string(), mock_inverter(2, 1000.0, 1500.0));

        // Let's test with linked_batteries = true first
        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: true,
            timezone: None,
            inverter: inverters.clone(),
            period: periods.clone(),
            grid_target: Some(0.0),
            initial_mode: Some("Auto".to_string()),
            tariff: None,
            demand: None,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        // Set capacity to 50% for both so they can discharge and charge
        pm.inverters.insert("solax1".to_string(), InverterState { battery_capacity: 50, ..Default::default() });
        pm.inverters.insert("solax2".to_string(), InverterState { battery_capacity: 50, ..Default::default() });

        // 1. In Auto mode (linked): grid power is 600W import, target is 0W.
        // error = 600.
        // total_discharge_power += 600 * 0.1 = 60W.
        // inv_state.discharge_power = 60 / 2 = 30W.
        // returns -30 for both.
        pm.total_power = 600.0;
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(-30));
        assert_eq!(cmd2, Some(-30));

        // 2. Transition to ChargeBatteries mode (linked)
        // should command max charge for each inverter (2000 and 1000)
        pm.mode = PowerManagerMode::ChargeBatteries;
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(2000));
        assert_eq!(cmd2, Some(1000));

        // 3. Transition to MaximumFeedin mode (linked)
        // should command max discharge for each inverter (3000 and 1500 -> negated is -3000 and -1500)
        pm.mode = PowerManagerMode::MaximumFeedin;
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(-3000));
        assert_eq!(cmd2, Some(-1500));

        // 4. Transition back to Auto (linked)
        // should continue Auto regulation from last total_discharge_power (60W).
        // Let's set grid power to -300W (feed in 300W).
        // error = -300.
        // total_discharge_power += -300 * 0.1 = -30W -> 60 - 30 = 30W.
        // inv_state.discharge_power = 30 / 2 = 15W.
        // returns -15 for both.
        pm.mode = PowerManagerMode::Auto;
        pm.total_power = -300.0;
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(-15));
        assert_eq!(cmd2, Some(-15));

        // Let's test with linked_batteries = false
        let config_unlinked = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            grid_target: Some(0.0),
            initial_mode: Some("Auto".to_string()),
            tariff: None,
            demand: None,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config_unlinked, "sensors".to_string());
        pm.inverters.insert("solax1".to_string(), InverterState { battery_capacity: 50, ..Default::default() });
        pm.inverters.insert("solax2".to_string(), InverterState { battery_capacity: 50, ..Default::default() });

        // 1. In Auto mode (unlinked): grid power is 400W import per phase.
        // Each inverter regulates independently:
        // error = 400.
        // discharge_power += 400 * 0.25 = 100W.
        // returns -100.
        pm.total_power = 800.0;
        pm.phase_power[1] = 400.0;
        pm.phase_power[2] = 400.0;
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(-100));
        assert_eq!(cmd2, Some(-100));

        // 2. Transition to ChargeBatteries mode (unlinked)
        // should command max charge for each (2000 and 1000)
        pm.mode = PowerManagerMode::ChargeBatteries;
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(2000));
        assert_eq!(cmd2, Some(1000));

        // 3. Transition to MaximumFeedin mode (unlinked)
        // should command max discharge for each (3000 and 1500 -> negated is -3000 and -1500)
        pm.mode = PowerManagerMode::MaximumFeedin;
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(-3000));
        assert_eq!(cmd2, Some(-1500));

        // 4. Transition back to Auto (unlinked)
        // Each inverter continues from its last modified state (which was max_discharge = 3000 and 1500).
        // Let's set phase powers to -400W (feed in 400W).
        // phase_error = -400. total_error = -800.
        // Each phase is managed independently.
        // solax1: 3000 - 100 = 2900W -> returns -2900.
        // solax2: 1500 - 100 = 1400W -> returns -1400.
        pm.mode = PowerManagerMode::Auto;
        pm.total_power = -800.0;
        pm.phase_power[1] = -400.0;
        pm.phase_power[2] = -400.0;
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(-2900));
        assert_eq!(cmd2, Some(-1400));
    }

    #[test]
    fn test_evaluate_and_command_smart_heuristic() {
        use crate::config::TariffConfig;

        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 3000.0));

        let tariff_config = TariffConfig::Amber {
            api_key: "api".to_string(),
            site_id: "site".to_string(),
            negative_export_prevent: true,
            low_price_charge: true,
            low_price_threshold: 10.0,
            high_price_discharge: true,
            high_price_threshold: 60.0,
            api_url: None,
        };

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            grid_target: Some(0.0),
            initial_mode: Some("SmartHeuristic".to_string()),
            tariff: Some(tariff_config),
            demand: None,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        assert_eq!(pm.mode, PowerManagerMode::SmartHeuristic);

        let state = InverterState {
            battery_capacity: 50,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        // Scenario 1: Negative export rate -> should charge from grid
        pm.tariff_manager
            .set_current_rates(crate::tariff_manager::CurrentTariffRates {
                import_rate: 5.0,
                export_rate: -2.0,
            });
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(2000));

        // Scenario 2: High export rate -> should discharge battery to grid
        pm.tariff_manager
            .set_current_rates(crate::tariff_manager::CurrentTariffRates {
                import_rate: 80.0,
                export_rate: 65.0,
            });
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-3000));

        // Scenario 3: Normal prices -> should fallback to Auto regulation (grid target 0)
        pm.tariff_manager
            .set_current_rates(crate::tariff_manager::CurrentTariffRates {
                import_rate: 30.0,
                export_rate: 15.0,
            });
        pm.phase_power[1] = 400.0;
        let state_reset = InverterState {
            battery_capacity: 50,
            discharge_power: 0.0,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_reset);
        pm.last_regulation_update = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-100));
    }

    #[test]
    fn test_calculate_aggregates_scenarios() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 3000.0));

        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());

        // Scenario A: Excess solar charging batteries (grid power charging = 0)
        let state_a = InverterState {
            battery_capacity: 50,
            pv1_power: 2000.0,
            pv2_power: 1500.0,
            battery_power: -1200.0, // charging
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_a);
        pm.total_power = -2000.0; // grid exporting

        let agg_a = pm.calculate_aggregates();
        assert_eq!(agg_a.get("Total Solar Production"), Some(&3500.0));
        assert_eq!(agg_a.get("Total Charging"), Some(&1200.0));
        assert_eq!(agg_a.get("Total Discharging"), Some(&0.0));
        assert_eq!(agg_a.get("Total Consumption"), Some(&300.0)); // 3500 - 2000 - 1200 = 300
        assert_eq!(agg_a.get("Total Grid Power Used for Charging"), Some(&0.0));

        // Scenario B: Solar insufficient, drawing from grid to charge batteries
        let state_b = InverterState {
            battery_capacity: 50,
            pv1_power: 500.0,
            pv2_power: 300.0,
            battery_power: -2500.0, // charging
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_b);
        pm.total_power = 3000.0; // grid importing

        let agg_b = pm.calculate_aggregates();
        assert_eq!(agg_b.get("Total Solar Production"), Some(&800.0));
        assert_eq!(agg_b.get("Total Charging"), Some(&2500.0));
        assert_eq!(agg_b.get("Total Discharging"), Some(&0.0));
        assert_eq!(agg_b.get("Total Consumption"), Some(&1300.0)); // 800 + 3000 - 2500 = 1300
        assert_eq!(
            agg_b.get("Total Grid Power Used for Charging"),
            Some(&2500.0)
        ); // min(3000, 2500)

        // Scenario C: No solar, battery discharging, grid import
        let state_c = InverterState {
            battery_capacity: 50,
            pv1_power: 0.0,
            pv2_power: 0.0,
            battery_power: 1500.0, // discharging
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state_c);
        pm.total_power = 1000.0; // grid importing

        let agg_c = pm.calculate_aggregates();
        assert_eq!(agg_c.get("Total Solar Production"), Some(&0.0));
        assert_eq!(agg_c.get("Total Charging"), Some(&0.0));
        assert_eq!(agg_c.get("Total Discharging"), Some(&1500.0));
        assert_eq!(agg_c.get("Total Consumption"), Some(&2500.0)); // 0 + 1000 + 1500 = 2500
        assert_eq!(agg_c.get("Total Grid Power Used for Charging"), Some(&0.0));
    }

    #[test]
    fn test_calculate_aggregates_scenarios_budget_and_usage() {
        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        let mut inv_cfg = mock_inverter(1, 2000.0, 3000.0);
        inv_cfg.max_discharge = 3000.0;
        inv_cfg.max_charge = 3000.0;
        inv_cfg.min_charge_pct = Some(15);
        inv_cfg.battery_capacity = Some(10.0);
        inverters.insert("solax1".to_string(), inv_cfg);

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
            battery_capacity: 50,
            pv1_power: 2000.0,
            pv2_power: 1500.0,
            battery_power: -1200.0, // charging
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);
        pm.total_power = -2000.0; // grid exporting

        let agg = pm.calculate_aggregates();
        assert_eq!(agg.get("Usage"), Some(&300.0));
        assert_eq!(agg.get("Power Budget"), Some(&3200.0));
    }

    #[test]
    fn test_evaluate_and_command_manual_override_no_matching_period() {
        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 3000.0));

        // Config has NO periods at all
        let config = SolaxBatteryControlConfig {
            source: None,
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: HashMap::new(),
            grid_target: Some(0.0),
            initial_mode: Some("ChargeBatteries".to_string()),
            tariff: None,
            demand: None,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        let state = InverterState {
            battery_capacity: 50,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        // 1. ChargeBatteries: should work even without a period!
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(2000));

        // 2. MaximumFeedin: should work even without a period!
        pm.mode = PowerManagerMode::MaximumFeedin;
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(-3000));

        // 3. Auto: should return None without a period
        pm.mode = PowerManagerMode::Auto;
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, None);
    }

    #[test]
    fn test_telemetry_history_db() {
        let temp_db = "temp_test_telemetry.db";
        let _ = std::fs::remove_file(temp_db);

        // 1. Initialize DB
        init_history_db(temp_db).unwrap();

        let now_ts = chrono::Utc::now().timestamp();
        // 2. Prepare some records
        let mut buffer = vec![
            HistoryRecord {
                timestamp: now_ts - 600,
                topic: "solax1/PV1 Power".to_string(),
                value: 1200.5,
            },
            HistoryRecord {
                timestamp: now_ts - 600,
                topic: "tariff/import_price".to_string(),
                value: 28.5,
            },
            HistoryRecord {
                timestamp: now_ts - 300,
                topic: "solax1/PV1 Power".to_string(),
                value: 1250.0,
            },
        ];

        // 3. Flush to DB (without pruning)
        flush_history_to_db(temp_db, &mut buffer, None);
        assert!(buffer.is_empty());

        // 4. Verify DB contents
        {
            let rows = crate::database::get_all_telemetry_since(temp_db, 0).unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].0, now_ts - 600);
            assert_eq!(rows[0].1, "solax1/PV1 Power");
            assert_eq!(rows[0].2, 1200.5);

            assert_eq!(rows[1].0, now_ts - 600);
            assert_eq!(rows[1].1, "tariff/import_price");
            assert_eq!(rows[1].2, 28.5);

            assert_eq!(rows[2].0, now_ts - 300);
            assert_eq!(rows[2].1, "solax1/PV1 Power");
            assert_eq!(rows[2].2, 1250.0);
        }

        // 5. Test pruning
        // Let's add records and run flush with 1-day retention
        let mut buffer2 = vec![
            HistoryRecord {
                timestamp: chrono::Utc::now().timestamp() - (2 * 24 * 3600), // 2 days ago (pruned)
                topic: "solax1/PV1 Power".to_string(),
                value: 500.0,
            },
            HistoryRecord {
                timestamp: chrono::Utc::now().timestamp() - 3600, // 1 hour ago (kept)
                topic: "solax1/PV1 Power".to_string(),
                value: 600.0,
            },
        ];
        flush_history_to_db(temp_db, &mut buffer2, Some(1));

        // Check total count - should be 4 (three original ones + 1 kept from buffer2, 1 pruned)
        {
            let total_count = crate::database::get_all_telemetry_since(temp_db, 0).unwrap().len();
            assert_eq!(total_count, 4);
        }

        let _ = std::fs::remove_file(temp_db);
    }

    #[test]
    fn test_calculate_percentiles() {
        // Even number of elements (10)
        let values = vec![10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0];
        // sorted: 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0
        // idx_30 = round(10 * 0.3) = 3 -> values[3] = 4.0
        // idx_70 = round(10 * 0.7) = 7 -> values[7] = 8.0
        let (p30, p70) = calculate_percentiles(values);
        assert_eq!(p30, 4.0);
        assert_eq!(p70, 8.0);

        // Single element
        let values_single = vec![42.0];
        let (p30, p70) = calculate_percentiles(values_single);
        assert_eq!(p30, 42.0);
        assert_eq!(p70, 42.0);
    }

    #[test]
    fn test_calculate_price_thresholds() {
        let temp_db = "temp_test_thresholds.db";
        let _ = std::fs::remove_file(temp_db);

        init_history_db(temp_db).unwrap();

        let now_ts = chrono::Utc::now().timestamp();
        let mut buffer = Vec::new();
        // Insert 10 import prices and 10 export prices to avoid fallback
        for i in 1..=10 {
            buffer.push(HistoryRecord {
                timestamp: now_ts - (i * 60),
                topic: "tariff/import_price".to_string(),
                value: i as f64 * 10.0, // 10.0 to 100.0
            });
            buffer.push(HistoryRecord {
                timestamp: now_ts - (i * 60),
                topic: "tariff/export_price".to_string(),
                value: i as f64 * 5.0, // 5.0 to 50.0
            });
        }
        flush_history_to_db(temp_db, &mut buffer, None);

        let thresholds = calculate_price_thresholds(temp_db).unwrap();
        // sorted import: 10, 20, 30, 40, 50, 60, 70, 80, 90, 100
        // idx_30 = round(10 * 0.3) = 3 -> value = 40.0
        // idx_70 = round(10 * 0.7) = 7 -> value = 80.0
        assert_eq!(thresholds.import_30, 40.0);
        assert_eq!(thresholds.import_70, 80.0);

        // sorted export: 5, 10, 15, 20, 25, 30, 35, 40, 45, 50
        // idx_30 = round(10 * 0.3) = 3 -> value = 20.0
        // idx_70 = round(10 * 0.7) = 7 -> value = 40.0
        assert_eq!(thresholds.export_30, 20.0);
        assert_eq!(thresholds.export_70, 40.0);

        let _ = std::fs::remove_file(temp_db);
    }

    #[test]
    fn test_custom_inverter_bounds() {
        let period = mock_period("00:00:00", "23:59:59", 20, false, false);
        let mut inverter = mock_inverter(1, 1500.0, 2000.0);
        inverter.min_charge_pct = Some(35);
        inverter.max_charge_pct = Some(90);

        // Capacity <= min_charge_pct (30 <= 35), trying to discharge (power > 0)
        let cmd1 = PowerManager::discharge_at(&inverter, &period, 500.0, 30);
        assert_eq!(cmd1, 0); // clamped to 0 because capacity (30) <= min_charge_pct (35)

        // Capacity > min_charge_pct (40 > 35), trying to discharge (power > 0)
        let cmd2 = PowerManager::discharge_at(&inverter, &period, 500.0, 40);
        assert_eq!(cmd2, -500); // allowed to discharge

        // Capacity >= max_charge_pct (92 >= 90), trying to charge (power < 0)
        let cmd3 = PowerManager::discharge_at(&inverter, &period, -500.0, 92);
        assert_eq!(cmd3, 0); // clamped to 0 because capacity (92) >= max_charge_pct (90)

        // Capacity < max_charge_pct (85 < 90), trying to charge (power < 0)
        let cmd4 = PowerManager::discharge_at(&inverter, &period, -500.0, 85);
        assert_eq!(cmd4, 500); // allowed to charge
    }

    #[test]
    fn test_evaluate_and_command_mpc_arbitrage() {
        let temp_db = "temp_test_mpc_arbitrage.db";
        let _ = std::fs::remove_file(temp_db);
        init_history_db(temp_db).unwrap();

        let mut buffer = Vec::new();
        let now = chrono::Local::now();

        // Populate 24 hours of telemetry history
        for h in 0..24 {
            let record_time = now - chrono::Duration::hours(24 - h);
            let ts = record_time.timestamp();
            let hour = record_time.time().hour();

            // Solar output during day (8:00 to 16:00)
            let solar = if hour >= 8 && hour < 16 { 2000.0 } else { 0.0 };
            // Load is 1000W at night (20:00 to 06:00), 500W during day
            let load = if hour >= 20 || hour < 6 { 1000.0 } else { 500.0 };
            // Price is cheap during day, expensive at night (40.0 vs 8.0)
            let import_price = if hour >= 20 || hour < 6 { 40.0 } else { 8.0 };

            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "solax1/PV1 Power".to_string(),
                value: solar,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "MainsMeter/Total system power".to_string(),
                value: load,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/import_price".to_string(),
                value: import_price,
            });
        }
        flush_history_to_db(temp_db, &mut buffer, None);

        let mut periods = HashMap::new();
        periods.insert(
            "Always".to_string(),
            mock_period("00:00:00", "23:59:59", 10, false, false),
        );

        let mut inverters = HashMap::new();
        inverters.insert("solax1".to_string(), mock_inverter(1, 2000.0, 3000.0));

        let tariff_config = crate::config::TariffConfig::Amber {
            api_key: "api".to_string(),
            site_id: "site".to_string(),
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            api_url: None,
        };

        let config = SolaxBatteryControlConfig {
            source: Some("MainsMeter".to_string()),
            linked_batteries: false,
            timezone: None,
            inverter: inverters,
            period: periods,
            grid_target: Some(0.0),
            initial_mode: Some("MpcArbitrage".to_string()),
            tariff: Some(tariff_config),
            demand: None,
            ..Default::default()
        };

        let mut pm = PowerManager::new(config, "sensors".to_string());
        pm.db_path = temp_db.to_string();

        assert_eq!(pm.mode, PowerManagerMode::MpcArbitrage);

        // Scenario 1: Current price is cheap (8.0 c/kWh), night average is expensive (40.0 c/kWh)
        // Night price is > 10% higher than charging rate, so reserve target includes night deficit (10 kWh deficit).
        // Battery SoC is 20% (approx 2.76 kWh < reserve threshold of ~72% / 10 kWh).
        // Grid pre-charging should trigger at max charge rate (2000W).
        pm.tariff_manager.set_current_rates(crate::tariff_manager::CurrentTariffRates {
            import_rate: 8.0,
            export_rate: 4.0,
        });

        let state = InverterState {
            battery_capacity: 20,
            ..Default::default()
        };
        pm.inverters.insert("solax1".to_string(), state);

        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(2000));

        // Scenario 2: Current price is not cheap (25.0 c/kWh)
        // Should fall back to default auto regulation (which returns None since no grid target correction is needed).
        pm.tariff_manager.set_current_rates(crate::tariff_manager::CurrentTariffRates {
            import_rate: 25.0,
            export_rate: 20.0,
        });
        pm.inverters.insert("solax1".to_string(), InverterState {
            battery_capacity: 20,
            discharge_power: 0.0,
            ..Default::default()
        });
        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(0));

        // Scenario 3: Flat rates, charging rate (8.0) is not more than 10% cheaper than night average (8.0)
        // Reserve target falls back to demand reserve (0 kWh), so target reserve pct = min_charge = 10%.
        // Since Battery SoC is 20% >= 10%, no grid pre-charging should occur.
        let _ = std::fs::remove_file(temp_db);
        init_history_db(temp_db).unwrap();
        buffer.clear();
        for h in 0..24 {
            let record_time = now - chrono::Duration::hours(24 - h);
            let ts = record_time.timestamp();
            let hour = record_time.time().hour();

            let solar = if hour >= 8 && hour < 16 { 2000.0 } else { 0.0 };
            let load = if hour >= 20 || hour < 6 { 1000.0 } else { 500.0 };
            let import_price = 8.0;

            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "solax1/PV1 Power".to_string(),
                value: solar,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "MainsMeter/Total system power".to_string(),
                value: load,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/import_price".to_string(),
                value: import_price,
            });
        }
        flush_history_to_db(temp_db, &mut buffer, None);
        pm.clear_metrics_cache();

        pm.tariff_manager.set_current_rates(crate::tariff_manager::CurrentTariffRates {
            import_rate: 8.0,
            export_rate: 4.0,
        });
        pm.inverters.insert("solax1".to_string(), InverterState {
            battery_capacity: 20,
            discharge_power: 0.0,
            ..Default::default()
        });

        let cmd = pm.evaluate_and_command("solax1");
        assert_eq!(cmd, Some(0));

        let _ = std::fs::remove_file(temp_db);
    }

    #[test]
    fn test_solar_math() {
        use chrono::TimeZone;
        let lat = -33.8688;
        let lon = 151.2093;
        // Solar position at noon
        let utc_dt = chrono::Utc.with_ymd_and_hms(2026, 6, 26, 2, 0, 0).unwrap(); // ~12pm Sydney time (UTC+10)
        let (elevation, azimuth) = calculate_solar_position(lat, lon, utc_dt);
        
        // Solar elevation should be positive during mid-day
        assert!(elevation > 0.0, "Elevation should be positive during mid-day");
        assert!(elevation < std::f64::consts::PI / 2.0);
        
        // Test POA calculation
        let dni = 800.0;
        let dhi = 150.0;
        let tilt = 20.0;
        let array_azimuth = 0.0; // facing North
        
        // If elevation is negative (night), POA should be 0
        let poa_night = calculate_poa_irradiance(dni, dhi, -0.1, azimuth, tilt, array_azimuth);
        assert_eq!(poa_night, 0.0);
        
        // Positive elevation should yield a valid POA
        let poa_day = calculate_poa_irradiance(dni, dhi, elevation, azimuth, tilt, array_azimuth);
        assert!(poa_day > 0.0);
        assert!(poa_day < dni + dhi);
    }

    #[test]
    fn test_forecast_loading_fallback() {
        let temp_db = "test_forecast_fallback.db";
        let _ = std::fs::remove_file(temp_db);
        init_history_db(temp_db).unwrap();
        
        let config = SolaxBatteryControlConfig {
            source: Some("MainsMeter".to_string()),
            demand: Some(crate::config::DemandConfig {
                start: "17:00:00".to_string(),
                end: "20:00:00".to_string(),
                rate: 30.0,
            }),
            ..Default::default()
        };
        let mut pm = PowerManager::new(config, "sensors".to_string());
        pm.db_path = temp_db.to_string();
        
        // Populate telemetry history for yesterday to see if get_persistence_metrics falls back to it.
        let now = chrono::Local::now().timestamp();
        let mut buffer = Vec::new();
        for h in 0..24 {
            let ts = now - 86400 + h * 3600;
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "solax1/PV1 Power".to_string(),
                value: 1000.0,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "MainsMeter/Total system power".to_string(),
                value: 500.0,
            });
        }
        flush_history_to_db(temp_db, &mut buffer, None);
        
        // 1. Fallback scenario (no forecast in DB)
        let metrics_fallback = pm.get_persistence_metrics();
        assert!(metrics_fallback.0 > 0.0, "Expected solar fallback yield to be > 0.0, got {}", metrics_fallback.0);
        
        // 2. Forecast scenario (forecast exists in DB)
        // Calculate the exact window get_persistence_metrics will query:
        let demand_window = get_demand_window(Some(&pm.config));
        let demand_start_time = demand_window.map(|(start, _)| start).unwrap_or_else(|| NaiveTime::from_hms_opt(17, 0, 0).unwrap());
        let mut end_dt = chrono::Local::now().date_naive().and_time(demand_start_time).and_local_timezone(chrono::Local).single().map(|dt| dt.timestamp()).unwrap_or(now);
        
        let start_ts = if now >= end_dt {
            let tomorrow = chrono::Local::now() + chrono::Duration::days(1);
            end_dt = tomorrow.date_naive().and_time(demand_start_time).and_local_timezone(chrono::Local).single().map(|dt| dt.timestamp()).unwrap_or(now);
            tomorrow.date_naive().and_time(NaiveTime::from_hms_opt(10, 0, 0).unwrap()).and_local_timezone(chrono::Local).single().map(|dt| dt.timestamp()).unwrap_or(now)
        } else {
            now
        };

        {
            let ts1 = start_ts;
            let ts2 = start_ts + 1800; // 30 mins later (well within window)
            assert!(ts2 <= end_dt, "Forecast test timestamps must be within the end_dt window bounds");
            crate::database::delete_and_save_solar_forecast(temp_db, &[(ts1, 5000.0), (ts2, 5000.0)]).unwrap();
        }
        pm.clear_metrics_cache();
        
        let metrics_forecast = pm.get_persistence_metrics();
        // Since forecast_used is true, expected_solar_kwh should match the forecast (5 kW * 0.5 hours = 2.5 kWh)
        assert!(metrics_forecast.0 > 0.0);
        assert!((metrics_forecast.0 - 2.5).abs() < 5e-3, "Expected 2.5 kWh solar forecast, got {}", metrics_forecast.0);
        
        let _ = std::fs::remove_file(temp_db);
    }

    #[test]
    fn test_suggest_optimal_thresholds() {
        let temp_db = "temp_test_suggestions.db";
        let _ = std::fs::remove_file(temp_db);

        init_history_db(temp_db).unwrap();

        let now_ts = chrono::Utc::now().timestamp();
        let mut buffer = Vec::new();

        // 1. Normal pricing scenario: 100 points
        // import prices from 1.0 to 100.0, export prices from 1.0 to 100.0
        // We insert them backwards to make sure sorting is tested.
        for i in 1..=100 {
            let ts = now_ts - (i * 60);
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/import_price".to_string(),
                value: i as f64,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/export_price".to_string(),
                value: i as f64,
            });
            buffer.push(HistoryRecord {
                timestamp: ts,
                topic: "MainsMeter/Total system power".to_string(),
                value: 1000.0,
            });
        }
        flush_history_to_db(temp_db, &mut buffer, None);

        let res = run_historical_simulation_impl(temp_db, "1d", None).unwrap();
        // sorted import_prices: 1.0, 2.0, ..., 100.0 (records.len() is 99)
        // median_import = import_prices[49] = 51.0
        // median_export = export_prices[49] = 51.0
        // 1st percentile: idx = 98 * 0.01 = 0 -> import_prices[0] = 2.0
        // capped at median_export: min(2.0, 51.0) = 2.0
        assert_eq!(res.suggest_charge_threshold, Some(2.0));

        // sorted export_prices: 1.0, 2.0, ..., 100.0 (records.len() is 99)
        // median_import = 51.0, median_export = 51.0
        // 3 * median_import = 153.0
        // 99th percentile: idx = 98 * 0.99 = 97 -> export_prices[97] = 99.0
        // max(99.0, 153.0) = 153.0
        assert_eq!(res.suggest_discharge_threshold, Some(153.0));

        let _ = std::fs::remove_file(temp_db);

        // 2. Safety bounds test case
        let temp_db_overlap = "temp_test_suggestions_overlap.db";
        let _ = std::fs::remove_file(temp_db_overlap);
        init_history_db(temp_db_overlap).unwrap();

        let mut buffer_overlap = Vec::new();
        // Let's insert 10 points: import prices all 20.0, export prices all 20.0
        for i in 1..=10 {
            let ts = now_ts - (i * 60);
            buffer_overlap.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/import_price".to_string(),
                value: 20.0,
            });
            buffer_overlap.push(HistoryRecord {
                timestamp: ts,
                topic: "tariff/export_price".to_string(),
                value: 20.0,
            });
            buffer_overlap.push(HistoryRecord {
                timestamp: ts,
                topic: "MainsMeter/Total system power".to_string(),
                value: 1000.0,
            });
        }
        flush_history_to_db(temp_db_overlap, &mut buffer_overlap, None);

        let res_overlap = run_historical_simulation_impl(temp_db_overlap, "1d", None).unwrap();
        // median import = 20.0, median export = 20.0
        // charge suggestion = 1st percentile of import (20.0) capped at median export (20.0) = 20.0
        // discharge suggestion = 99th percentile of export (20.0) floored at 3x median import (60.0) = 60.0
        assert_eq!(res_overlap.suggest_discharge_threshold, Some(60.0));
        assert_eq!(res_overlap.suggest_charge_threshold, Some(20.0));

        let _ = std::fs::remove_file(temp_db_overlap);
    }

    #[test]
    fn test_grid_target_persistence_in_db() {
        let temp_db = "temp_test_grid_target_persistence.db";
        let _ = std::fs::remove_file(temp_db);

        // 1. Save config with grid_target
        let mut cfg = crate::config::Config::default_empty();
        let mut bat_ctrl = crate::config::SolaxBatteryControlConfig::default();
        bat_ctrl.grid_target = Some(-250.0);
        cfg.battery_control = Some(bat_ctrl);

        cfg.save_to_db(temp_db).unwrap();

        // 2. Reload config from DB and verify grid_target persistence
        let loaded_cfg = crate::config::Config::load_from_db(temp_db).unwrap();
        let bat_ctrl_loaded = loaded_cfg.battery_control.as_ref().unwrap();
        assert_eq!(bat_ctrl_loaded.grid_target, Some(-250.0));

        // 3. Verify PowerManager initializes grid_target from reloaded config
        let pm = PowerManager::new(bat_ctrl_loaded.clone(), "sensors".to_string());
        assert_eq!(pm.grid_target, -250.0);

        let _ = std::fs::remove_file(temp_db);
    }
}


