use crate::config::{
    BatteryControlInverter, BatteryControlPeriod, MqttBrokerConfig, SolaxBatteryControlConfig,
};
use crate::mqtt_helper::create_mqtt_client;
use chrono::{Local, NaiveTime, Timelike, Utc, Datelike, TimeZone};
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

#[derive(Debug, Clone)]
struct HistoryRecord {
    timestamp: i64,
    topic: String,
    value: f64,
}

pub fn init_history_db(db_path: &str) -> Result<(), rusqlite::Error> {
    let conn = rusqlite::Connection::open(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS telemetry_history (
            timestamp INTEGER NOT NULL,
            topic TEXT NOT NULL,
            value REAL NOT NULL,
            PRIMARY KEY (timestamp, topic)
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_history_timestamp ON telemetry_history (timestamp)",
        [],
    )?;
    Ok(())
}

fn flush_history_to_db(db_path: &str, buffer: &mut Vec<HistoryRecord>, retention_days: Option<u32>) {
    if buffer.is_empty() {
        return;
    }
    let mut conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to open DB for telemetry flush: {}", e);
            return;
        }
    };
    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to start transaction for telemetry flush: {}", e);
            return;
        }
    };
    {
        let mut stmt = match tx.prepare_cached(
            "INSERT OR REPLACE INTO telemetry_history (timestamp, topic, value) VALUES (?1, ?2, ?3)"
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to prepare flush statement: {}", e);
                return;
            }
        };
        for record in buffer.iter() {
            if let Err(e) = stmt.execute(rusqlite::params![record.timestamp, record.topic, record.value]) {
                eprintln!("Failed to execute telemetry insert: {}", e);
            }
        }
    }
    if let Err(e) = tx.commit() {
        eprintln!("Failed to commit telemetry flush transaction: {}", e);
        return;
    }
    println!("Flushed {} telemetry records to SQLite database", buffer.len());
    buffer.clear();

    if let Some(days) = retention_days {
        let cutoff = Utc::now().timestamp() - (days as i64 * 24 * 3600);
        if let Err(e) = conn.execute("DELETE FROM telemetry_history WHERE timestamp < ?1", rusqlite::params![cutoff]) {
            eprintln!("Failed to prune old telemetry records: {}", e);
        }
    }
}

fn get_price_history(db_path: &str, topic: &str, since_timestamp: i64) -> Result<Vec<f64>, rusqlite::Error> {
    let conn = rusqlite::Connection::open(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT value FROM telemetry_history WHERE topic = ?1 AND timestamp >= ?2"
    )?;
    let rows = stmt.query_map(rusqlite::params![topic, since_timestamp], |row| {
        row.get(0)
    })?;
    let mut values = Vec::new();
    for val in rows {
        if let Ok(v) = val {
            values.push(v);
        }
    }
    Ok(values)
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
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to open DB for capacity inference: {}", e);
            return None;
        }
    };

    let capacity_topic = format!("{}/Battery Capacity", inverter_name);
    let power_topic = format!("{}/Battery Power", inverter_name);
    let thirty_days_ago = Utc::now().timestamp() - (30 * 24 * 3600);

    let mut stmt = match conn.prepare(
        "SELECT timestamp, topic, value 
         FROM telemetry_history 
         WHERE (topic = ?1 OR topic = ?2) AND timestamp >= ?3 
         ORDER BY timestamp ASC"
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to prepare SQL statement for capacity inference: {}", e);
            return None;
        }
    };

    let rows = match stmt.query_map(rusqlite::params![capacity_topic, power_topic, thirty_days_ago], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to execute SQL query for capacity inference: {}", e);
            return None;
        }
    };

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

    for row in rows {
        if let Ok((ts, topic, val)) = row {
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

    fn evaluate_auto_regulate(
        &mut self,
        inverter_name: &str,
        inverter_config: &BatteryControlInverter,
        period: &BatteryControlPeriod,
        inv_state: &mut InverterState,
        num_inverters: f64,
    ) -> i32 {
        if self.linked_batteries {
            let any_low_capacity = self.config.inverter.keys().any(|name| {
                let limit = self.config.inverter.get(name)
                    .and_then(|c| c.min_charge_pct)
                    .unwrap_or(period.min_charge);
                self.inverters.get(name)
                    .map(|inv| inv.battery_capacity < limit)
                    .unwrap_or(false)
            });

            if period.grid_charge && any_low_capacity {
                inv_state.discharge_power = -inverter_config.max_charge;
                self.assist_needed.insert(inverter_name.to_string(), false);
                Self::discharge_at(
                    inverter_config,
                    period,
                    inv_state.discharge_power,
                    inv_state.battery_capacity,
                )
            } else if any_low_capacity && period.prefer_battery {
                let total_pv: f64 = self.config.inverter.keys().map(|name| {
                    self.inverters.get(name)
                        .map(|inv| inv.pv1_power + inv.pv2_power)
                        .unwrap_or(0.0)
                }).sum();
                inv_state.discharge_power = -total_pv / num_inverters;
                if inv_state.discharge_power < -inverter_config.max_charge {
                    inv_state.discharge_power = -inverter_config.max_charge;
                }
                self.assist_needed.insert(inverter_name.to_string(), true);
                Self::discharge_at(
                    inverter_config,
                    period,
                    inv_state.discharge_power,
                    inv_state.battery_capacity,
                )
            } else {
                let total_error = self.total_power - self.grid_target;
                let now = std::time::Instant::now();
                if now.duration_since(self.last_regulation_update).as_secs_f64() >= 1.0 {
                    self.total_discharge_power += total_error * 0.1;
                    if self.total_discharge_power > self.max_total_discharge_power {
                        self.total_discharge_power = self.max_total_discharge_power;
                    } else if self.total_discharge_power < -self.max_total_charge_power {
                        self.total_discharge_power = -self.max_total_charge_power;
                    }
                    self.last_regulation_update = now;
                }
                inv_state.discharge_power = self.total_discharge_power / num_inverters;

                let min_limit = inverter_config.min_charge_pct.unwrap_or(period.min_charge);
                if inv_state.battery_capacity <= min_limit && inv_state.discharge_power > 0.0 {
                    inv_state.discharge_power = 0.0;
                    self.assist_needed.insert(inverter_name.to_string(), true);
                } else {
                    let assist_needed_val = *self.assist_needed.get(inverter_name).unwrap_or(&false);
                    if assist_needed_val {
                        let lower_limit = inverter_config.single_phase_discharge_limit / num_inverters;
                        let upper_limit = -inverter_config.single_phase_charge_limit / num_inverters;
                        if (inv_state.discharge_power >= 0.0 && inv_state.discharge_power < lower_limit)
                            || (inv_state.discharge_power < 0.0 && inv_state.discharge_power > upper_limit)
                        {
                            self.assist_needed.insert(inverter_name.to_string(), false);
                        }
                    } else {
                        let val = inv_state.discharge_power + total_error * 0.75 / num_inverters;
                        if val > inverter_config.single_phase_discharge_limit
                            || val < -inverter_config.single_phase_charge_limit
                        {
                            self.assist_needed.insert(inverter_name.to_string(), true);
                        }
                    }
                }

                println!(
                    "DEBUG [{}] (linked) total_power={}, total_discharge_power={}, inv_state.discharge_power={}, assist_needed={:?}",
                    inverter_name,
                    self.total_power,
                    self.total_discharge_power,
                    inv_state.discharge_power,
                    self.assist_needed
                );

                if inv_state.discharge_power > inverter_config.max_discharge {
                    inv_state.discharge_power = inverter_config.max_discharge;
                } else if inv_state.discharge_power < -inverter_config.max_charge {
                    inv_state.discharge_power = -inverter_config.max_charge;
                }

                let max_limit = inverter_config.max_charge_pct.unwrap_or(95);
                if inv_state.battery_capacity > max_limit
                    && inv_state.discharge_power < 0.0
                    && inv_state.battery_power > (inv_state.discharge_power / 10.0)
                {
                    inv_state.discharge_power = 0.0;
                    self.assist_needed.insert(inverter_name.to_string(), true);
                }

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

                Self::discharge_at(
                    inverter_config,
                    period,
                    inv_state.discharge_power,
                    inv_state.battery_capacity,
                )
            }
        } else {
            let min_limit = inverter_config.min_charge_pct.unwrap_or(period.min_charge);
            if period.grid_charge && inv_state.battery_capacity < min_limit {
                inv_state.discharge_power = -inverter_config.max_charge;
                self.assist_needed.insert(inverter_name.to_string(), false);
                Self::discharge_at(
                    inverter_config,
                    period,
                    inv_state.discharge_power,
                    inv_state.battery_capacity,
                )
            } else if inv_state.battery_capacity < min_limit && period.prefer_battery {
                inv_state.discharge_power = -inv_state.pv1_power - inv_state.pv2_power;
                if inv_state.discharge_power < -inverter_config.max_charge {
                    inv_state.discharge_power = -inverter_config.max_charge;
                }
                self.assist_needed.insert(inverter_name.to_string(), true);
                Self::discharge_at(
                    inverter_config,
                    period,
                    inv_state.discharge_power,
                    inv_state.battery_capacity,
                )
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
                let assist_needed_val = *self.assist_needed.get(inverter_name).unwrap_or(&false);
                if assist_needed_val {
                    let lower_limit = inverter_config.single_phase_discharge_limit / num_inverters;
                    let upper_limit = -inverter_config.single_phase_charge_limit / num_inverters;
                    if (inv_state.discharge_power >= 0.0 && inv_state.discharge_power < lower_limit)
                        || (inv_state.discharge_power < 0.0 && inv_state.discharge_power > upper_limit)
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
                let min_limit = inverter_config.min_charge_pct.unwrap_or(period.min_charge);
                if inv_state.battery_capacity <= min_limit && inv_state.discharge_power > 0.0 {
                    inv_state.discharge_power = 0.0;
                    self.assist_needed.insert(inverter_name.to_string(), true);
                    Self::discharge_at(
                        inverter_config,
                        period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    )
                } else {
                    // Assistance power load share
                    let any_assist = self.assist_needed.values().any(|&v| v);
                    if any_assist {
                        let phase_error = phase_power_val - (self.grid_target / num_inverters);
                        inv_state.discharge_power -= phase_error * 0.25;
                        let total_error = self.total_power - self.grid_target;
                        inv_state.discharge_power += total_error * 0.1;
                    }

                    println!(
                        "DEBUG [{}] total_power={}, total_discharge_power={}, inv_state.discharge_power={}, any_assist={}, assist_needed={:?}",
                        inverter_name,
                        self.total_power,
                        self.total_discharge_power,
                        inv_state.discharge_power,
                        self.assist_needed.values().any(|&v| v),
                        self.assist_needed
                    );

                    // Clamp values
                    if inv_state.discharge_power > inverter_config.max_discharge {
                        inv_state.discharge_power = inverter_config.max_discharge;
                    } else if inv_state.discharge_power < -inverter_config.max_charge {
                        inv_state.discharge_power = -inverter_config.max_charge;
                    }

                    // BMS Throttling
                    let max_limit = inverter_config.max_charge_pct.unwrap_or(95);
                    if inv_state.battery_capacity > max_limit
                        && inv_state.discharge_power < 0.0
                        && inv_state.battery_power > (inv_state.discharge_power / 10.0)
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
                        } else if inv_state.discharge_power < -inverter_config.grace_charge_power {
                            inv_state.discharge_power = -inverter_config.grace_charge_power;
                        }
                    }

                    Self::discharge_at(
                        inverter_config,
                        period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    )
                }
            }
        }
    }

    pub fn evaluate_and_command(&mut self, inverter_name: &str) -> Option<i32> {
        let inverter_config = self.config.inverter.get(inverter_name)?.clone();
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
        };

        // Ensure state entry exists
        if !self.inverters.contains_key(inverter_name) {
            self.inverters
                .insert(inverter_name.to_string(), InverterState::default());
        }

        let mut inv_state = self.inverters.get(inverter_name).unwrap().clone();
        let num_inverters = self.config_inverters_count as f64;

        let charge_val_opt = match self.mode {
            PowerManagerMode::ChargeBatteries => {
                let period = period_opt.unwrap_or(default_period);
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
                let period = period_opt.unwrap_or(default_period);
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
                let period = period_opt?;
                let charge_val = self.evaluate_auto_regulate(
                    inverter_name,
                    &inverter_config,
                    &period,
                    &mut inv_state,
                    num_inverters,
                );
                Some(charge_val)
            }
            PowerManagerMode::SmartHeuristic => {
                let period = period_opt?;
                let rates = self.tariff_manager.get_current_rates();
                let import_rate = rates.import_rate;
                let export_rate = rates.export_rate;

                let now = chrono::Local::now();
                let now_time = now.time();
                let demand_window = get_demand_window(Some(&self.config));

                let capacity_kwh = inverter_config.battery_capacity.unwrap_or(13.8);
                let reserve_kwh = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                let reserve_pct = ((reserve_kwh / capacity_kwh) * 100.0) as u8;

                // 1. Extreme negative price or negative export price: charge from grid
                let negative_export_triggered = if let Some(crate::config::TariffConfig::Amber { negative_export_prevent, .. }) = self.tariff_manager.config() {
                    *negative_export_prevent && export_rate < 0.0
                } else {
                    false
                };
                if import_rate < 0.0 || negative_export_triggered {
                    let max_limit = inverter_config.max_charge_pct.unwrap_or(100);
                    if inv_state.battery_capacity < max_limit {
                        inv_state.discharge_power = -inverter_config.max_charge;
                    } else {
                        inv_state.discharge_power = 0.0;
                    }
                    self.assist_needed.insert(inverter_name.to_string(), false);
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                }
                // 2. High export price: dump to grid (arbitrage)
                else if export_rate >= 30.0 && inv_state.battery_capacity > reserve_pct {
                    let min_limit = inverter_config.min_charge_pct.unwrap_or(period.min_charge);
                    if inv_state.battery_capacity > min_limit {
                        inv_state.discharge_power = inverter_config.max_discharge;
                        self.assist_needed.insert(inverter_name.to_string(), true);
                    } else {
                        inv_state.discharge_power = 0.0;
                        self.assist_needed.insert(inverter_name.to_string(), false);
                    }
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                }
                // 3. Pre-charge window: top up using cheap grid
                else if demand_window.map_or(false, |(start, _)| now_time.hour() >= 10 && now_time < start) && import_rate < 15.0 && inv_state.battery_capacity < 85 {
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
                // 4. Default grid regulation fallback (shaves peak to 0 during demand window)
                else {
                    let charge_val = self.evaluate_auto_regulate(
                        inverter_name,
                        &inverter_config,
                        &period,
                        &mut inv_state,
                        num_inverters,
                    );
                    Some(charge_val)
                }
            }
            PowerManagerMode::AdaptivePeakShaving => {
                let period = period_opt?;
                let rates = self.tariff_manager.get_current_rates();
                let import_rate = rates.import_rate;
                let export_rate = rates.export_rate;

                let now = chrono::Local::now();
                let now_time = now.time();
                let demand_window = get_demand_window(Some(&self.config));

                let capacity_kwh = inverter_config.battery_capacity.unwrap_or(13.8);
                let reserve_kwh = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                let reserve_pct = ((reserve_kwh / capacity_kwh) * 100.0) as u8;

                // 1. Extreme negative price or negative export price: charge from grid
                let negative_export_triggered = if let Some(crate::config::TariffConfig::Amber { negative_export_prevent, .. }) = self.tariff_manager.config() {
                    *negative_export_prevent && export_rate < 0.0
                } else {
                    false
                };
                if import_rate < 0.0 || negative_export_triggered {
                    let max_limit = inverter_config.max_charge_pct.unwrap_or(100);
                    if inv_state.battery_capacity < max_limit {
                        inv_state.discharge_power = -inverter_config.max_charge;
                    } else {
                        inv_state.discharge_power = 0.0;
                    }
                    self.assist_needed.insert(inverter_name.to_string(), false);
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                }
                // 2. High export price: dump to grid (arbitrage)
                else if export_rate >= 30.0 && inv_state.battery_capacity > reserve_pct {
                    let min_limit = inverter_config.min_charge_pct.unwrap_or(period.min_charge);
                    if inv_state.battery_capacity > min_limit {
                        inv_state.discharge_power = inverter_config.max_discharge;
                        self.assist_needed.insert(inverter_name.to_string(), true);
                    } else {
                        inv_state.discharge_power = 0.0;
                        self.assist_needed.insert(inverter_name.to_string(), false);
                    }
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                }
                // 3. Peak demand window shaving
                else if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                    let monthly_peak = self.get_monthly_peak_draw();
                    let orig_target = self.grid_target;
                    self.grid_target = monthly_peak;

                    let charge_val = self.evaluate_auto_regulate(
                        inverter_name,
                        &inverter_config,
                        &period,
                        &mut inv_state,
                        num_inverters,
                    );

                    self.grid_target = orig_target;
                    Some(charge_val)
                }
                // 4. Default grid regulation fallback
                else {
                    let charge_val = self.evaluate_auto_regulate(
                        inverter_name,
                        &inverter_config,
                        &period,
                        &mut inv_state,
                        num_inverters,
                    );
                    Some(charge_val)
                }
            }
            PowerManagerMode::MpcOptimizer => {
                let period = period_opt?;
                let rates = self.tariff_manager.get_current_rates();
                let import_rate = rates.import_rate;
                let export_rate = rates.export_rate;

                let now = chrono::Local::now();
                let now_time = now.time();
                let demand_window = get_demand_window(Some(&self.config));

                let capacity_kwh = inverter_config.battery_capacity.unwrap_or(13.8);
                let (expected_solar, demand_needed, cheap_threshold) = self.get_persistence_metrics();

                let required_reserve = demand_needed.min(capacity_kwh * 0.95);
                let reserve_pct = ((required_reserve / capacity_kwh) * 100.0) as u8;
                let reserve_pct = reserve_pct.max(period.min_charge);

                // 1. Extreme negative price or negative export price: charge from grid
                let negative_export_triggered = if let Some(crate::config::TariffConfig::Amber { negative_export_prevent, .. }) = self.tariff_manager.config() {
                    *negative_export_prevent && export_rate < 0.0
                } else {
                    false
                };
                if import_rate < 0.0 || negative_export_triggered {
                    let max_limit = inverter_config.max_charge_pct.unwrap_or(100);
                    if inv_state.battery_capacity < max_limit {
                        inv_state.discharge_power = -inverter_config.max_charge;
                    } else {
                        inv_state.discharge_power = 0.0;
                    }
                    self.assist_needed.insert(inverter_name.to_string(), false);
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                }
                // 2. High export price: dump to grid (arbitrage)
                else if export_rate >= 30.0 && inv_state.battery_capacity > (reserve_pct + 10) {
                    let min_limit = inverter_config.min_charge_pct.unwrap_or(period.min_charge);
                    if inv_state.battery_capacity > min_limit {
                        inv_state.discharge_power = inverter_config.max_discharge;
                        self.assist_needed.insert(inverter_name.to_string(), true);
                    } else {
                        inv_state.discharge_power = 0.0;
                        self.assist_needed.insert(inverter_name.to_string(), false);
                    }
                    let charge_val = Self::discharge_at(
                        &inverter_config,
                        &period,
                        inv_state.discharge_power,
                        inv_state.battery_capacity,
                    );
                    Some(charge_val)
                }
                // 3. Pre-charge if projected deficit exists
                else if demand_window.map_or(false, |(start, _)| now_time < start) && (inv_state.battery_capacity as f64 / 100.0 * capacity_kwh + expected_solar) < required_reserve {
                    let is_cheap = import_rate < 12.0 || import_rate <= cheap_threshold;
                    if is_cheap {
                        inv_state.discharge_power = -inverter_config.max_charge;
                        self.assist_needed.insert(inverter_name.to_string(), false);
                        let charge_val = Self::discharge_at(
                            &inverter_config,
                            &period,
                            inv_state.discharge_power,
                            inv_state.battery_capacity,
                        );
                        Some(charge_val)
                    } else {
                        let charge_val = self.evaluate_auto_regulate(
                            inverter_name,
                            &inverter_config,
                            &period,
                            &mut inv_state,
                            num_inverters,
                        );
                        Some(charge_val)
                    }
                }
                // 4. Default grid regulation fallback (shaves peak to 0 during demand window)
                else {
                    let charge_val = self.evaluate_auto_regulate(
                        inverter_name,
                        &inverter_config,
                        &period,
                        &mut inv_state,
                        num_inverters,
                    );
                    Some(charge_val)
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

        let min_limit = in_cfg.min_charge_pct.unwrap_or(period.min_charge);
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

        aggregates
    }

    fn get_monthly_peak_draw(&self) -> f64 {
        let demand_window = get_demand_window(Some(&self.config));
        if demand_window.is_none() {
            return 0.0;
        }
        let (demand_start, demand_end) = demand_window.unwrap();
        let mains_source = self.config.source.as_deref().unwrap_or("MainsMeter");
        let conn = match rusqlite::Connection::open(&self.db_path) {
            Ok(c) => c,
            Err(_) => return 0.0,
        };

        let now = chrono::Local::now();
        let start_of_month = match chrono::Local.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0) {
            chrono::LocalResult::Single(t) => t,
            _ => return 0.0,
        };
        let start_epoch = start_of_month.timestamp();

        let topic = format!("{}/Total system power", mains_source);
        let mut stmt = match conn.prepare(
            "SELECT timestamp, value FROM telemetry_history WHERE topic = ?1 AND timestamp >= ?2"
        ) {
            Ok(s) => s,
            Err(_) => return 0.0,
        };

        let mut peak = 0.0;
        if let Ok(mut rows) = stmt.query(rusqlite::params![topic, start_epoch]) {
            while let Ok(Some(row)) = rows.next() {
                let ts: i64 = row.get(0).unwrap_or(0);
                let val: f64 = row.get(1).unwrap_or(0.0);

                let dt = chrono::Utc.timestamp_opt(ts, 0)
                    .single()
                    .map(|utc| utc.with_timezone(&chrono::Local))
                    .unwrap_or_else(|| chrono::Local::now());

                let dt_time = dt.time();
                if is_time_in_window(dt_time, demand_start, demand_end) && val > peak {
                    peak = val;
                }
            }
        }
        peak
    }

    fn get_persistence_metrics(&self) -> (f64, f64, f64) {
        let demand_window = get_demand_window(Some(&self.config));
        let mains_source = self.config.source.as_deref().unwrap_or("MainsMeter");
        let conn = match rusqlite::Connection::open(&self.db_path) {
            Ok(c) => c,
            Err(_) => return (0.0, 0.0, 12.0),
        };

        let now = chrono::Local::now().timestamp();
        let since = now - 86400; // 24 hours ago

        let mut stmt = match conn.prepare(
            "SELECT timestamp, topic, value FROM telemetry_history WHERE timestamp >= ?1 AND timestamp <= ?2 ORDER BY timestamp ASC"
        ) {
            Ok(s) => s,
            Err(_) => return (0.0, 0.0, 12.0),
        };

        use std::collections::BTreeMap;
        struct RawGroup {
            solar: f64,
            load: Option<f64>,
            import_price: Option<f64>,
        }
        let mut groups: BTreeMap<i64, RawGroup> = BTreeMap::new();

        if let Ok(mut rows) = stmt.query(rusqlite::params![since, now]) {
            while let Ok(Some(row)) = rows.next() {
                let ts: i64 = row.get(0).unwrap_or(0);
                let topic: String = row.get(1).unwrap_or_default();
                let val: f64 = row.get(2).unwrap_or(0.0);

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
        }

        let keys: Vec<i64> = groups.keys().cloned().collect();
        if keys.len() < 2 {
            return (0.0, 0.0, 12.0);
        }

        let mut expected_solar_kwh = 0.0;
        let mut demand_energy_needed_kwh = 0.0;
        let mut import_prices = Vec::new();

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
                .map(|utc| utc.with_timezone(&chrono::Local))
                .unwrap_or_else(|| chrono::Local::now());
            let dt_time = dt.time();

            if demand_window.map_or(false, |(start, _)| dt_time < start) {
                expected_solar_kwh += (solar / 1000.0) * duration_hours;
            }

            if demand_window.map_or(false, |(start, end)| is_time_in_window(dt_time, start, end)) {
                let net_power = load - solar;
                if net_power > 0.0 {
                    demand_energy_needed_kwh += (net_power / 1000.0) * duration_hours;
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

        (expected_solar_kwh, demand_energy_needed_kwh, cheap_threshold_price)
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

pub async fn run_power_manager_task(
    config: SolaxBatteryControlConfig,
    mqtt_config: MqttBrokerConfig,
    cancel_token: CancellationToken,
    db_path: String,
) {
    let history_config = crate::config::Config::load_from_db(&db_path)
        .ok()
        .and_then(|c| c.history);
    let history_enabled = history_config.as_ref().map(|h| h.enabled).unwrap_or(false);
    let flush_interval_mins = history_config.as_ref().map(|h| h.flush_interval_mins).unwrap_or(30);
    let retention_days = history_config.as_ref().and_then(|h| h.retention_days);

    if history_enabled {
        if let Err(e) = init_history_db(&db_path) {
            eprintln!("Failed to initialize telemetry history table: {}", e);
        }
    }

    let mut latest_telemetry: HashMap<String, f64> = HashMap::new();
    let mut history_buffer: Vec<HistoryRecord> = Vec::new();
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

                    let now_ts = Utc::now().timestamp();
                    for (topic, &value) in &latest_telemetry {
                        history_buffer.push(HistoryRecord {
                            timestamp: now_ts,
                            topic: topic.clone(),
                            value,
                        });
                    }

                    if last_flush.elapsed() >= Duration::from_secs(flush_interval_mins as u64 * 60) {
                        flush_history_to_db(&db_path, &mut history_buffer, retention_days);
                        last_flush = std::time::Instant::now();
                    }
                }
            }
            res = eventloop.poll() => {
                match res {
                    Ok(notification) => {
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
                                                pm_lock.config.grid_target = Some(target);
                                                println!(
                                                    "Power Manager grid target changed to: {}W",
                                                    target
                                                );

                                                if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                                    status.grid_target = target;
                                                }

                                                // Save to SQLite DB
                                                if let Ok(mut db_cfg) = crate::config::Config::load_from_db(&db_path) {
                                                    if let Some(ref mut bat_ctrl) = db_cfg.battery_control {
                                                        bat_ctrl.grid_target = Some(target);
                                                        if let Err(e) = db_cfg.save_to_db(&db_path) {
                                                            eprintln!("Failed to save config to DB on grid target change: {}", e);
                                                        }
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
                                        if history_enabled {
                                            latest_telemetry.insert(format!("{}/{}", device_name, metric), val);
                                        }
                                        let mut pm_lock = pm.lock().await;
                                        let mut state_changed = false;

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

                                        if state_changed {
                                            let aggregates = pm_lock.calculate_aggregates();
                                            drop(pm_lock);

                                            for (metric_name, value) in aggregates {
                                                let topic = format!("{}/aggregate/{}", base_topic, metric_name);
                                                let _ = mqtt_client
                                                    .publish(&topic, QoS::AtMostOnce, false, value.to_string())
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
                    Err(e) => {
                        println!("Power Manager MQTT error: {}", e);
                        if currently_connected {
                            currently_connected = false;
                            if let Ok(mut status) = crate::web_server::get_system_status().lock() {
                                status.mqtt_connected = false;
                            }
                        }
                        tokio::select! {
                            _ = cancel_token.cancelled() => break,
                            _ = sleep(Duration::from_secs(5)) => {}
                        }
                    }
                }
            }
        }
    }

    if history_enabled && !history_buffer.is_empty() {
        println!("Shutting down Power Manager. Flushing remaining {} telemetry records to DB...", history_buffer.len());
        flush_history_to_db(&db_path, &mut history_buffer, retention_days);
    }
}

#[derive(serde::Serialize, Clone, Default)]
pub struct SimulationResultModel {
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub cycles: f64,
    pub energy_cost: f64,
    pub demand_charges: f64,
    pub net_bill: f64,
}

#[derive(serde::Serialize, Clone)]
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
}

pub fn run_historical_simulation(db_path: &str, range: &str) -> Result<SimulationResponse, String> {
    run_historical_simulation_impl(db_path, range, None)
}

pub fn run_historical_simulation_impl(
    db_path: &str,
    range: &str,
    progress_cb: Option<&(dyn Fn(f64, f64) + Send + Sync)>,
) -> Result<SimulationResponse, String> {
    let config = crate::config::Config::load_from_db(db_path).unwrap_or_else(|_| crate::config::Config::default_empty());
    let demand_window = get_demand_window(config.battery_control.as_ref());
    let demand_rate = config.battery_control.as_ref()
        .and_then(|bc| bc.demand.as_ref())
        .map(|d| d.rate)
        .unwrap_or(0.0);
    let mains_source = config.battery_control.as_ref()
        .and_then(|bc| bc.source.as_deref())
        .unwrap_or("MainsMeter");

    let mut battery_capacity_kwh = 0.0;
    let mut max_power_w = 0.0;
    let mut min_charge_pct = 20;
    let mut max_charge_pct = 100;

    if let Some(ref bc) = config.battery_control {
        std::thread::scope(|s| {
            let mut threads = Vec::new();
            for (inv_name, inv_cfg) in &bc.inverter {
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

    let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;

    let now_ts = chrono::Local::now().timestamp();
    let start_ts = match range {
        "1d" => now_ts - 86400,
        "1w" => now_ts - 7 * 86400,
        "1m" => now_ts - 30 * 86400,
        "1y" => now_ts - 365 * 86400,
        _ => 0,
    };

    let mut stmt = conn.prepare(
        "SELECT timestamp, topic, value FROM telemetry_history WHERE timestamp >= ?1 ORDER BY timestamp ASC"
    ).map_err(|e| e.to_string())?;

    use std::collections::BTreeMap;
    struct SimTempGroup {
        solar: f64,
        load: Option<f64>,
        battery: f64,
        import_price: Option<f64>,
        export_price: Option<f64>,
    }
    let mut groups: BTreeMap<i64, SimTempGroup> = BTreeMap::new();

    let mut rows = stmt.query(rusqlite::params![start_ts]).map_err(|e| e.to_string())?;
    while let Ok(Some(row)) = rows.next() {
        let ts: i64 = row.get(0).unwrap_or(0);
        let topic: String = row.get(1).unwrap_or_default();
        let val: f64 = row.get(2).unwrap_or(0.0);

        let entry = groups.entry(ts).or_insert(SimTempGroup {
            solar: 0.0,
            load: None,
            battery: 0.0,
            import_price: None,
            export_price: None,
        });

        if topic.ends_with("/PV1 Power") || topic.ends_with("/PV2 Power") || topic.contains("/Input 1 Power") || topic.contains("/Input 2 Power") {
            entry.solar += val;
        } else if topic == format!("{}/Total system power", mains_source) {
            entry.load = Some(val);
        } else if topic.ends_with("/Battery Power") {
            entry.battery += val;
        } else if topic == "tariff/import_price" {
            entry.import_price = Some(val);
        } else if topic == "tariff/export_price" {
            entry.export_price = Some(val);
        }
    }

    let keys: Vec<i64> = groups.keys().cloned().collect();
    if keys.len() < 2 {
        return Err("Insufficient historical telemetry data in database to run simulation.".to_string());
    }

    #[derive(Clone)]
    struct SimCleanRecord {
        timestamp: i64,
        dt_local: chrono::DateTime<chrono::Local>,
        solar_power_w: f64,
        load_power_w: f64,
        import_price_cents: f64,
        export_price_cents: f64,
        duration_hours: f64,
    }

    let mut records = Vec::new();
    for i in 0..keys.len() - 1 {
        let ts = keys[i];
        let next_ts = keys[i + 1];
        let duration_hours = (next_ts - ts) as f64 / 3600.0;
        if duration_hours > 2.0 {
            continue;
        }

        let g = &groups[&ts];
        if g.load.is_none() {
            continue;
        }

        let dt_local = chrono::Utc.timestamp_opt(ts, 0)
            .single()
            .map(|utc| utc.with_timezone(&chrono::Local))
            .unwrap_or_else(|| chrono::Local::now());

        let grid_w = g.load.unwrap();
        let gross_load_w = (grid_w + g.solar + g.battery).max(0.0);

        records.push(SimCleanRecord {
            timestamp: ts,
            dt_local,
            solar_power_w: g.solar,
            load_power_w: gross_load_w,
            import_price_cents: g.import_price.unwrap_or(0.0),
            export_price_cents: g.export_price.unwrap_or(0.0),
            duration_hours,
        });
    }

    if records.is_empty() {
        return Err("No aligned telemetry records found for simulation in range.".to_string());
    }

    let mut last_imp = 25.0;
    let mut last_exp = 8.0;
    let mut last_imp_age = 9999;
    let mut last_exp_age = 9999;

    for r in &mut records {
        if r.import_price_cents > 0.0 {
            last_imp = r.import_price_cents;
            last_imp_age = 0;
        } else {
            if last_imp_age < 4 {
                r.import_price_cents = last_imp;
                last_imp_age += 1;
            } else {
                r.import_price_cents = 25.0;
                last_imp_age = 9999;
            }
        }

        if r.export_price_cents > 0.0 {
            last_exp = r.export_price_cents;
            last_exp_age = 0;
        } else {
            if last_exp_age < 4 {
                r.export_price_cents = last_exp;
                last_exp_age += 1;
            } else {
                r.export_price_cents = 8.0;
                last_exp_age = 9999;
            }
        }
    }

    fn calculate_demand_charges_total(monthly_peaks: &HashMap<String, f64>, rate: f64) -> f64 {
        let mut total = 0.0;
        for peak_w in monthly_peaks.values() {
            let peak_kw = peak_w / 1000.0;
            total += peak_kw * rate * 30.0;
        }
        total
    }

    let start_date = records.first().unwrap().dt_local.format("%Y-%m-%d %H:%M:%S").to_string();
    let end_date = records.last().unwrap().dt_local.format("%Y-%m-%d %H:%M:%S").to_string();
    let records_simulated = records.len();

    let records_ref = &records;

    let (no_battery_res, baseline_res, auto_res, smart_heuristic_res, lookahead_mpc_res, adaptive_peak_res) =
        std::thread::scope(|s| {
            let t_no_bat = s.spawn(move || {
                let mut no_bat_import_kwh = 0.0;
                let mut no_bat_export_kwh = 0.0;
                let mut no_bat_energy_cost = 0.0;
                let mut no_bat_peaks = HashMap::new();

                for r in records_ref {
                    let net_w = r.load_power_w - r.solar_power_w;
                    let now_time = r.dt_local.time();
                    let month_key = r.dt_local.format("%Y-%m").to_string();

                    if net_w > 0.0 {
                        let kwh = (net_w / 1000.0) * r.duration_hours;
                        no_bat_import_kwh += kwh;
                        no_bat_energy_cost += kwh * (r.import_price_cents / 100.0);

                        if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                            let peak = no_bat_peaks.entry(month_key).or_insert(0.0);
                            if net_w > *peak {
                                *peak = net_w;
                            }
                        }
                    } else {
                        let kwh = (-net_w / 1000.0) * r.duration_hours;
                        no_bat_export_kwh += kwh;
                        no_bat_energy_cost -= kwh * (r.export_price_cents / 100.0);
                    }
                }
                let no_bat_demand = calculate_demand_charges_total(&no_bat_peaks, demand_rate);
                SimulationResultModel {
                    import_kwh: no_bat_import_kwh,
                    export_kwh: no_bat_export_kwh,
                    cycles: 0.0,
                    energy_cost: no_bat_energy_cost,
                    demand_charges: no_bat_demand,
                    net_bill: no_bat_energy_cost + no_bat_demand,
                }
            });

            let t_base = s.spawn(move || {
                let mut base_import_kwh = 0.0;
                let mut base_export_kwh = 0.0;
                let mut base_energy_cost = 0.0;
                let mut base_peaks = HashMap::new();
                let mut base_cycles = 0.0;
                let mut bat_soc = battery_capacity_kwh * 0.5;

                for r in records_ref {
                    let net_w = r.load_power_w - r.solar_power_w;
                    let now_time = r.dt_local.time();
                    let month_key = r.dt_local.format("%Y-%m").to_string();

                    let net_grid_w;
                    if net_w > 0.0 {
                        let max_avail_discharge = (bat_soc * 0.95) / r.duration_hours * 1000.0;
                        let discharge = net_w.min(max_power_w).min(max_avail_discharge);

                        bat_soc -= (discharge / 1000.0) * r.duration_hours / 0.95;
                        base_cycles += (discharge / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w - discharge;
                    } else {
                        let max_avail_charge = ((battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                        let charge = (-net_w).min(max_power_w).min(max_avail_charge);

                        bat_soc += (charge / 1000.0) * r.duration_hours * 0.95;
                        base_cycles += (charge / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w + charge;
                    }

                    if net_grid_w > 0.0 {
                        let kwh = (net_grid_w / 1000.0) * r.duration_hours;
                        base_import_kwh += kwh;
                        base_energy_cost += kwh * (r.import_price_cents / 100.0);

                        if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                            let peak = base_peaks.entry(month_key).or_insert(0.0);
                            if net_grid_w > *peak {
                                *peak = net_grid_w;
                            }
                        }
                    } else {
                        let kwh = (-net_grid_w / 1000.0) * r.duration_hours;
                        base_export_kwh += kwh;
                        base_energy_cost -= kwh * (r.export_price_cents / 100.0);
                    }
                }
                let base_demand = calculate_demand_charges_total(&base_peaks, demand_rate);
                SimulationResultModel {
                    import_kwh: base_import_kwh,
                    export_kwh: base_export_kwh,
                    cycles: base_cycles,
                    energy_cost: base_energy_cost,
                    demand_charges: base_demand,
                    net_bill: base_energy_cost + base_demand,
                }
            });

            let t_auto = s.spawn(move || {
                let mut auto_import_kwh = 0.0;
                let mut auto_export_kwh = 0.0;
                let mut auto_energy_cost = 0.0;
                let mut auto_peaks = HashMap::new();
                let mut auto_cycles = 0.0;
                let mut bat_soc = battery_capacity_kwh * 0.5;

                for r in records_ref {
                    let net_w = r.load_power_w - r.solar_power_w;
                    let month_key = r.dt_local.format("%Y-%m").to_string();

                    let mut charge_w = 0.0;
                    let mut discharge_w = 0.0;

                    let now_time = r.dt_local.time();

                    // Match period
                    let mut active_period = None;
                    if let Some(ref bc) = config.battery_control {
                        for period in bc.period.values() {
                            if let (Ok(start), Ok(end)) = (
                                NaiveTime::parse_from_str(&period.start, "%H:%M:%S"),
                                NaiveTime::parse_from_str(&period.end, "%H:%M:%S"),
                            ) {
                                if start < end {
                                    if now_time >= start && now_time < end {
                                        active_period = Some(period);
                                        break;
                                    }
                                } else {
                                    if !(now_time >= end && now_time < start) {
                                        active_period = Some(period);
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    let min_pct = active_period.map(|p| p.min_charge).unwrap_or(min_charge_pct) as f64;
                    let grid_charge = active_period.map(|p| p.grid_charge).unwrap_or(false);
                    let prefer_battery = active_period.map(|p| p.prefer_battery).unwrap_or(false);
                    let force_discharge = active_period.and_then(|p| p.force_discharge);

                    let bat_pct = (bat_soc / battery_capacity_kwh) * 100.0;

                    if let Some(fd_w) = force_discharge {
                        if fd_w > 0.0 {
                            let max_avail_discharge = ((bat_soc - (min_pct / 100.0) * battery_capacity_kwh).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = fd_w.min(max_power_w).min(max_avail_discharge);
                        } else if fd_w < 0.0 {
                            let max_avail_charge = (((battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-fd_w).min(max_power_w).min(max_avail_charge);
                        }
                    } else if grid_charge && bat_pct < min_pct {
                        let max_avail_charge = (((battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                        charge_w = max_power_w.min(max_avail_charge);
                    } else if prefer_battery && bat_pct < min_pct {
                        if net_w < 0.0 {
                            let max_avail_charge = (((battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        }
                    } else {
                        if net_w > 0.0 {
                            let max_avail_discharge = ((bat_soc - (min_pct / 100.0) * battery_capacity_kwh).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = net_w.min(max_power_w).min(max_avail_discharge);
                        } else {
                            let max_avail_charge = (((battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        }
                    }

                    let net_grid_w;
                    if charge_w > 0.0 {
                        bat_soc += (charge_w / 1000.0) * r.duration_hours * 0.95;
                        auto_cycles += (charge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w + charge_w;
                    } else if discharge_w > 0.0 {
                        bat_soc -= (discharge_w / 1000.0) * r.duration_hours / 0.95;
                        auto_cycles += (discharge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w - discharge_w;
                    } else {
                        net_grid_w = net_w;
                    }

                    if net_grid_w > 0.0 {
                        let kwh = (net_grid_w / 1000.0) * r.duration_hours;
                        auto_import_kwh += kwh;
                        auto_energy_cost += kwh * (r.import_price_cents / 100.0);

                        if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                            let peak = auto_peaks.entry(month_key).or_insert(0.0);
                            if net_grid_w > *peak {
                                *peak = net_grid_w;
                            }
                        }
                    } else {
                        let kwh = (-net_grid_w / 1000.0) * r.duration_hours;
                        auto_export_kwh += kwh;
                        auto_energy_cost -= kwh * (r.export_price_cents / 100.0);
                    }
                }
                let auto_demand = calculate_demand_charges_total(&auto_peaks, demand_rate);
                SimulationResultModel {
                    import_kwh: auto_import_kwh,
                    export_kwh: auto_export_kwh,
                    cycles: auto_cycles,
                    energy_cost: auto_energy_cost,
                    demand_charges: auto_demand,
                    net_bill: auto_energy_cost + auto_demand,
                }
            });

            let t_smart = s.spawn(move || {
                let mut smart_import_kwh = 0.0;
                let mut smart_export_kwh = 0.0;
                let mut smart_energy_cost = 0.0;
                let mut smart_peaks = HashMap::new();
                let mut smart_cycles = 0.0;
                let mut bat_soc = battery_capacity_kwh * 0.5;

                for r in records_ref {
                    let net_w = r.load_power_w - r.solar_power_w;
                    let now_time = r.dt_local.time();
                    let month_key = r.dt_local.format("%Y-%m").to_string();
                    let import_price = r.import_price_cents;
                    let export_price = r.export_price_cents;

                    let is_demand = demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end));
                    let is_pre_charge = demand_window.map_or(false, |(start, _)| now_time.hour() >= 10 && now_time < start);

                    let mut charge_w = 0.0;
                    let mut discharge_w = 0.0;

                    if import_price < 0.0 {
                        let max_avail_charge = ((battery_capacity_kwh * (max_charge_pct as f64 / 100.0) - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                        charge_w = max_power_w.min(max_avail_charge.max(0.0));
                    } else if export_price >= 30.0 {
                        let reserve = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                        if bat_soc > reserve {
                            let max_avail_discharge = ((bat_soc - reserve) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = max_power_w.min(max_avail_discharge.max(0.0));
                        }
                    } else if is_pre_charge && import_price < 15.0 && (bat_soc / battery_capacity_kwh) < 0.85 {
                        let target = battery_capacity_kwh * 0.85;
                        let deficit = target - bat_soc;
                        let max_avail_charge = (deficit / 0.95) / r.duration_hours * 1000.0;
                        charge_w = max_power_w.min(max_avail_charge.max(0.0));
                    } else if is_demand {
                        if net_w > 0.0 {
                            let min_pct_limit = battery_capacity_kwh * (min_charge_pct as f64 / 100.0);
                            let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = net_w.min(max_power_w).min(max_avail_discharge);
                        } else {
                            let max_avail_charge = ((battery_capacity_kwh * (max_charge_pct as f64 / 100.0) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        }
                    } else {
                        if net_w > 0.0 {
                            let min_pct_limit = battery_capacity_kwh * (min_charge_pct as f64 / 100.0);
                            let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = net_w.min(max_power_w).min(max_avail_discharge);
                        } else {
                            let max_avail_charge = ((battery_capacity_kwh * (max_charge_pct as f64 / 100.0) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        }
                    }

                    let net_grid_w;
                    if charge_w > 0.0 {
                        bat_soc += (charge_w / 1000.0) * r.duration_hours * 0.95;
                        smart_cycles += (charge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w + charge_w;
                    } else if discharge_w > 0.0 {
                        bat_soc -= (discharge_w / 1000.0) * r.duration_hours / 0.95;
                        smart_cycles += (discharge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w - discharge_w;
                    } else {
                        net_grid_w = net_w;
                    }

                    if net_grid_w > 0.0 {
                        let kwh = (net_grid_w / 1000.0) * r.duration_hours;
                        smart_import_kwh += kwh;
                        smart_energy_cost += kwh * (import_price / 100.0);

                        if is_demand {
                            let peak = smart_peaks.entry(month_key).or_insert(0.0);
                            if net_grid_w > *peak {
                                *peak = net_grid_w;
                            }
                        }
                    } else {
                        let kwh = (-net_grid_w / 1000.0) * r.duration_hours;
                        smart_export_kwh += kwh;
                        smart_energy_cost -= kwh * (export_price / 100.0);
                    }
                }
                let smart_demand = calculate_demand_charges_total(&smart_peaks, demand_rate);
                SimulationResultModel {
                    import_kwh: smart_import_kwh,
                    export_kwh: smart_export_kwh,
                    cycles: smart_cycles,
                    energy_cost: smart_energy_cost,
                    demand_charges: smart_demand,
                    net_bill: smart_energy_cost + smart_demand,
                }
            });

            let t_mpc = s.spawn(move || {
                let mut mpc_import_kwh = 0.0;
                let mut mpc_export_kwh = 0.0;
                let mut mpc_energy_cost = 0.0;
                let mut mpc_peaks = HashMap::new();
                let mut mpc_cycles = 0.0;
                let mut bat_soc = battery_capacity_kwh * 0.5;

                let progress_step = (records_simulated / 100).max(1);
                let start_time = std::time::Instant::now();

                for i in 0..records_simulated {
                    if i % progress_step == 0 {
                        if let Some(cb) = progress_cb {
                            let elapsed = start_time.elapsed().as_secs_f64();
                            let percent = (i as f64 / records_simulated as f64) * 100.0;
                            let eta_seconds = if i > 0 {
                                elapsed * (records_simulated as f64 - i as f64) / i as f64
                            } else {
                                0.0
                            };
                            cb(percent, eta_seconds);
                        }
                    }

                    let r = &records_ref[i];
                    let net_w = r.load_power_w - r.solar_power_w;
                    let now_time = r.dt_local.time();
                    let month_key = r.dt_local.format("%Y-%m").to_string();
                    let import_price = r.import_price_cents;
                    let export_price = r.export_price_cents;

                    let is_demand = demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end));

                    let mut expected_solar = 0.0;
                    let mut demand_needed = 0.0;
                    let mut cheapest_future = Vec::new();

                    for j in i..records_simulated {
                        let fr = &records_ref[j];
                        if fr.timestamp - r.timestamp > 86400 {
                            break;
                        }
                        let ftime = fr.dt_local.time();
                        let fnet = fr.load_power_w - fr.solar_power_w;

                        if demand_window.map_or(false, |(start, end)| is_time_in_window(ftime, start, end)) && fnet > 0.0 {
                            demand_needed += (fnet / 1000.0) * fr.duration_hours;
                        }
                        if demand_window.map_or(false, |(start, _)| ftime < start) && fnet < 0.0 {
                            expected_solar += (-fnet / 1000.0) * fr.duration_hours;
                        }
                        cheapest_future.push((j, fr.import_price_cents));
                    }

                    cheapest_future.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                    let threshold_idx = (cheapest_future.len() / 5).max(1);
                    let cheap_threshold = cheapest_future[threshold_idx - 1].1;

                    let required_reserve = (demand_needed / 0.95).min(battery_capacity_kwh * 0.95);

                    let mut charge_w = 0.0;
                    let mut discharge_w = 0.0;

                    if is_demand {
                        if net_w > 0.0 {
                            let max_avail_discharge = (bat_soc * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = net_w.min(max_power_w).min(max_avail_discharge);
                        } else {
                            let max_avail_charge = ((battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        }
                    } else {
                        let projected_deficit = required_reserve - (bat_soc + expected_solar * 0.95);
                        let is_cheap = import_price < 12.0 || import_price <= cheap_threshold;

                        if projected_deficit > 0.0 && is_cheap {
                            let max_avail_charge = (projected_deficit / 0.95) / r.duration_hours * 1000.0;
                            charge_w = max_power_w.min(max_avail_charge.max(0.0));
                        } else if net_w < 0.0 {
                            let max_avail_charge = ((battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        } else if export_price >= 30.0 && bat_soc > (required_reserve + battery_capacity_kwh * 0.1) {
                            let max_avail_discharge = ((bat_soc - required_reserve) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = max_power_w.min(max_avail_discharge.max(0.0));
                        } else if net_w > 0.0 {
                            let available = (bat_soc - required_reserve).max(0.0);
                            let max_avail_discharge = (available * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = net_w.min(max_power_w).min(max_avail_discharge);
                        }
                    }

                    let net_grid_w;
                    if charge_w > 0.0 {
                        bat_soc += (charge_w / 1000.0) * r.duration_hours * 0.95;
                        mpc_cycles += (charge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w + charge_w;
                    } else if discharge_w > 0.0 {
                        bat_soc -= (discharge_w / 1000.0) * r.duration_hours / 0.95;
                        mpc_cycles += (discharge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w - discharge_w;
                    } else {
                        net_grid_w = net_w;
                    }

                    if net_grid_w > 0.0 {
                        let kwh = (net_grid_w / 1000.0) * r.duration_hours;
                        mpc_import_kwh += kwh;
                        mpc_energy_cost += kwh * (import_price / 100.0);

                        if is_demand {
                            let peak = mpc_peaks.entry(month_key).or_insert(0.0);
                            if net_grid_w > *peak {
                                *peak = net_grid_w;
                            }
                        }
                    } else {
                        let kwh = (-net_grid_w / 1000.0) * r.duration_hours;
                        mpc_export_kwh += kwh;
                        mpc_energy_cost -= kwh * (export_price / 100.0);
                    }
                }

                if let Some(cb) = progress_cb {
                    cb(100.0, 0.0);
                }

                let mpc_demand = calculate_demand_charges_total(&mpc_peaks, demand_rate);
                SimulationResultModel {
                    import_kwh: mpc_import_kwh,
                    export_kwh: mpc_export_kwh,
                    cycles: mpc_cycles,
                    energy_cost: mpc_energy_cost,
                    demand_charges: mpc_demand,
                    net_bill: mpc_energy_cost + mpc_demand,
                }
            });

            let t_adapt = s.spawn(move || {
                let mut adapt_import_kwh = 0.0;
                let mut adapt_export_kwh = 0.0;
                let mut adapt_energy_cost = 0.0;
                let mut adapt_peaks = HashMap::new();
                let mut adapt_cycles = 0.0;
                let mut bat_soc = battery_capacity_kwh * 0.5;

                for r in records_ref {
                    let net_w = r.load_power_w - r.solar_power_w;
                    let now_time = r.dt_local.time();
                    let month_key = r.dt_local.format("%Y-%m").to_string();
                    let import_price = r.import_price_cents;
                    let export_price = r.export_price_cents;

                    let is_demand = demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end));
                    let current_month_peak = *adapt_peaks.get(&month_key).unwrap_or(&0.0);

                    let mut charge_w = 0.0;
                    let mut discharge_w = 0.0;

                    if import_price < 0.0 {
                        let max_avail_charge = ((battery_capacity_kwh * (max_charge_pct as f64 / 100.0) - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                        charge_w = max_power_w.min(max_avail_charge.max(0.0));
                    } else if export_price >= 30.0 {
                        let reserve = if demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start - chrono::Duration::hours(3), end)) { 5.0 } else { 2.0 };
                        if bat_soc > reserve {
                            let max_avail_discharge = ((bat_soc - reserve) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = max_power_w.min(max_avail_discharge.max(0.0));
                        }
                    } else if is_demand {
                        if net_w > current_month_peak {
                            let excess = net_w - current_month_peak;
                            let max_avail_discharge = (bat_soc * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = excess.min(max_power_w).min(max_avail_discharge);
                        } else if net_w < 0.0 {
                            let max_avail_charge = ((battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        }
                    } else {
                        if net_w > 0.0 {
                            let min_pct_limit = battery_capacity_kwh * (min_charge_pct as f64 / 100.0);
                            let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                            discharge_w = net_w.min(max_power_w).min(max_avail_discharge);
                        } else {
                            let max_avail_charge = ((battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                            charge_w = (-net_w).min(max_power_w).min(max_avail_charge);
                        }
                    }

                    let net_grid_w;
                    if charge_w > 0.0 {
                        bat_soc += (charge_w / 1000.0) * r.duration_hours * 0.95;
                        adapt_cycles += (charge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w + charge_w;
                    } else if discharge_w > 0.0 {
                        bat_soc -= (discharge_w / 1000.0) * r.duration_hours / 0.95;
                        adapt_cycles += (discharge_w / 1000.0) * r.duration_hours / battery_capacity_kwh;
                        net_grid_w = net_w - discharge_w;
                    } else {
                        net_grid_w = net_w;
                    }

                    if net_grid_w > 0.0 {
                        let kwh = (net_grid_w / 1000.0) * r.duration_hours;
                        adapt_import_kwh += kwh;
                        adapt_energy_cost += kwh * (import_price / 100.0);

                        if is_demand {
                            let peak = adapt_peaks.entry(month_key).or_insert(0.0);
                            if net_grid_w > *peak {
                                *peak = net_grid_w;
                            }
                        }
                    } else {
                        let kwh = (-net_grid_w / 1000.0) * r.duration_hours;
                        adapt_export_kwh += kwh;
                        adapt_energy_cost -= kwh * (export_price / 100.0);
                    }
                }
                let adapt_demand = calculate_demand_charges_total(&adapt_peaks, demand_rate);
                SimulationResultModel {
                    import_kwh: adapt_import_kwh,
                    export_kwh: adapt_export_kwh,
                    cycles: adapt_cycles,
                    energy_cost: adapt_energy_cost,
                    demand_charges: adapt_demand,
                    net_bill: adapt_energy_cost + adapt_demand,
                }
            });

            (
                t_no_bat.join().unwrap(),
                t_base.join().unwrap(),
                t_auto.join().unwrap(),
                t_smart.join().unwrap(),
                t_mpc.join().unwrap(),
                t_adapt.join().unwrap(),
            )
        });

    let mut total_solar_kwh = 0.0;
    let mut total_usage_kwh = 0.0;
    for r in &records {
        total_solar_kwh += (r.solar_power_w / 1000.0) * r.duration_hours;
        total_usage_kwh += (r.load_power_w / 1000.0) * r.duration_hours;
    }

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
            battery_capacity: None,
            max_charge_pct: None,
            min_charge_pct: None,
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
        assert_eq!(cmd1, 1500);
        assert_eq!(cmd2, 1500);

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
        // Since any_assist is true, the net change on each inverter is total_error * 0.1 = -80W.
        // solax1: 3000 - 80 = 2920W -> returns -2920.
        // solax2: 1500 - 80 = 1420W -> returns -1420.
        pm.mode = PowerManagerMode::Auto;
        pm.total_power = -800.0;
        pm.phase_power[1] = -400.0;
        pm.phase_power[2] = -400.0;
        let cmd1 = pm.evaluate_and_command("solax1");
        let cmd2 = pm.evaluate_and_command("solax2");
        assert_eq!(cmd1, Some(-2920));
        assert_eq!(cmd2, Some(-1420));
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
            let conn = rusqlite::Connection::open(temp_db).unwrap();
            let mut stmt = conn.prepare("SELECT timestamp, topic, value FROM telemetry_history ORDER BY timestamp, topic").unwrap();
            let mut rows = stmt.query([]).unwrap();

            let r1 = rows.next().unwrap().unwrap();
            assert_eq!(r1.get::<_, i64>(0).unwrap(), now_ts - 600);
            assert_eq!(r1.get::<_, String>(1).unwrap(), "solax1/PV1 Power");
            assert_eq!(r1.get::<_, f64>(2).unwrap(), 1200.5);

            let r2 = rows.next().unwrap().unwrap();
            assert_eq!(r2.get::<_, i64>(0).unwrap(), now_ts - 600);
            assert_eq!(r2.get::<_, String>(1).unwrap(), "tariff/import_price");
            assert_eq!(r2.get::<_, f64>(2).unwrap(), 28.5);

            let r3 = rows.next().unwrap().unwrap();
            assert_eq!(r3.get::<_, i64>(0).unwrap(), now_ts - 300);
            assert_eq!(r3.get::<_, String>(1).unwrap(), "solax1/PV1 Power");
            assert_eq!(r3.get::<_, f64>(2).unwrap(), 1250.0);
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
            let conn = rusqlite::Connection::open(temp_db).unwrap();
            let total_count: i64 = conn.query_row("SELECT COUNT(*) FROM telemetry_history", [], |r| r.get(0)).unwrap();
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
}

