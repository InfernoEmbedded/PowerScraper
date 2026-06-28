use crate::power_manager::DailyScenarioResult;
use crate::config::EvolvedHeuristicConfig;
use std::collections::{BTreeMap, HashMap};
use chrono::{DateTime, FixedOffset, NaiveTime};

#[derive(Clone, Debug)]
pub struct SimRecord {
    pub timestamp: i64,
    pub dt_local: DateTime<FixedOffset>,
    pub solar_power_w: f64,
    pub load_power_w: f64,
    pub import_price_cents: f64,
    pub export_price_cents: f64,
    pub duration_hours: f64,
    pub day_solar_kwh: f64,
}

#[derive(Clone)]
pub struct SimConfig {
    pub battery_capacity_kwh: f64,
    pub max_power_w: f64,
    pub min_charge_pct: u8,
    pub max_charge_pct: u8,
    pub demand_window: Option<(NaiveTime, NaiveTime)>,
    pub demand_rate: f64,
    pub negative_export_prevent: bool,
    pub low_price_charge: bool,
    pub low_price_threshold: f64,
    pub high_price_discharge: bool,
    pub high_price_threshold: f64,
    pub periods: Vec<crate::config::BatteryControlPeriod>,
    pub evolved_heuristic: EvolvedHeuristicConfig,
    pub evolved_heuristic_monthly: Option<std::collections::HashMap<String, EvolvedHeuristicConfig>>,
}

pub struct SimTracker {
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub cycles: f64,
    pub energy_cost: f64,
    pub daily: BTreeMap<String, DailyScenarioResult>,
    pub soc_history: Vec<f32>,
    pub grid_history: Vec<f32>,
}

impl SimTracker {
    pub fn new() -> Self {
        Self {
            import_kwh: 0.0,
            export_kwh: 0.0,
            cycles: 0.0,
            energy_cost: 0.0,
            daily: BTreeMap::new(),
            soc_history: Vec::new(),
            grid_history: Vec::new(),
        }
    }

    pub fn record_step(
        &mut self,
        date: &str,
        net_grid_w: f64,
        duration_hours: f64,
        step_cycles: f64,
        import_price: f64,
        export_price: f64,
        bat_soc: f64,
    ) {
        self.soc_history.push(bat_soc as f32);
        self.grid_history.push(net_grid_w as f32);

        let day = self.daily.entry(date.to_string()).or_default();
        day.cycles += step_cycles;
        self.cycles += step_cycles;

        if net_grid_w > 0.0 {
            let kwh = (net_grid_w / 1000.0) * duration_hours;
            self.import_kwh += kwh;
            let cost = kwh * (import_price / 100.0);
            self.energy_cost += cost;
            day.import_kwh += kwh;
            day.energy_cost += cost;
        } else {
            let kwh = (-net_grid_w / 1000.0) * duration_hours;
            self.export_kwh += kwh;
            let credit = kwh * (export_price / 100.0);
            self.energy_cost -= credit;
            day.export_kwh += kwh;
            day.energy_cost -= credit;
        }
    }
}

pub fn calculate_demand_charges_total(monthly_peaks: &HashMap<String, f64>, rate: f64) -> f64 {
    let mut total = 0.0;
    for peak_w in monthly_peaks.values() {
        let peak_kw = peak_w / 1000.0;
        total += peak_kw * rate * 30.0;
    }
    total
}

pub fn is_time_in_window(now_time: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start < end {
        now_time >= start && now_time < end
    } else {
        !(now_time >= end && now_time < start)
    }
}

pub mod no_battery;
pub mod baseline;
pub mod auto;
pub mod smart_heuristic;
pub mod lookahead_mpc;
pub mod adaptive_peak;
pub mod mpc_arbitrage;
pub mod evolved_heuristic;
pub mod tuning;
