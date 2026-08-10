use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use chrono::Timelike;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut smart_peaks = HashMap::new();
    let mut bat_soc = config.battery_capacity_kwh * 0.5;
    let mut tracker = SimTracker::new();

    for r in records {
        let net_w = r.load_power_w - r.solar_power_w;
        let now_time = r.dt_local.time();
        let month_key = r.dt_local.format("%Y-%m").to_string();
        let date_str = r.dt_local.date_naive().to_string();
        let import_price = r.import_price_cents;
        let export_price = r.export_price_cents;

        let is_demand = config.demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end));
        let is_pre_charge = config.demand_window.map_or(false, |(start, _)| now_time.hour() >= 10 && now_time < start);

        let mut charge_w = 0.0;
        let mut discharge_w = 0.0;

        let negative_export_triggered = config.negative_export_prevent && export_price < 0.0;
        if import_price < 0.0 || negative_export_triggered {
            let max_avail_charge = ((config.battery_capacity_kwh * (config.max_charge_pct as f64 / 100.0) - bat_soc) / 0.95) / r.duration_hours * 1000.0;
            charge_w = config.max_power_w.min(max_avail_charge.max(0.0));
        } else if config.high_price_discharge && export_price >= config.high_price_threshold {
            let reserve = if let Some((start, end)) = config.demand_window {
                let start_minus_3 = start - chrono::Duration::hours(3);
                if is_time_in_window(now_time, start_minus_3, end) { 5.0 } else { 2.0 }
            } else {
                2.0
            };
            if bat_soc > reserve {
                let max_avail_discharge = ((bat_soc - reserve) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = config.max_power_w.min(max_avail_discharge.max(0.0));
            }
        } else if is_pre_charge && (config.low_price_charge && import_price <= config.low_price_threshold || config.demand_rate > 0.0) && (bat_soc / config.battery_capacity_kwh) < 0.85 {
            let target = config.battery_capacity_kwh * 0.85;
            let deficit = target - bat_soc;
            let max_avail_charge = (deficit / 0.95) / r.duration_hours * 1000.0;
            charge_w = config.max_power_w.min(max_avail_charge.max(0.0));
        } else if is_demand {
            if net_w > 0.0 {
                let min_pct_limit = config.battery_capacity_kwh * (config.min_charge_pct as f64 / 100.0);
                let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = net_w.min(config.max_power_w).min(max_avail_discharge);
            } else {
                let max_avail_charge = ((config.battery_capacity_kwh * (config.max_charge_pct as f64 / 100.0) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        } else {
            if net_w > 0.0 {
                let min_pct_limit = config.battery_capacity_kwh * (config.min_charge_pct as f64 / 100.0);
                let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = net_w.min(config.max_power_w).min(max_avail_discharge);
            } else {
                let max_avail_charge = ((config.battery_capacity_kwh * (config.max_charge_pct as f64 / 100.0) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        }

        let net_grid_w;
        let mut step_cycles = 0.0;
        if charge_w > 0.0 {
            bat_soc += (charge_w / 1000.0) * r.duration_hours * 0.95;
            step_cycles = (charge_w / 1000.0) * r.duration_hours / config.battery_capacity_kwh;
            net_grid_w = net_w + charge_w;
        } else if discharge_w > 0.0 {
            bat_soc -= (discharge_w / 1000.0) * r.duration_hours / 0.95;
            step_cycles = (discharge_w / 1000.0) * r.duration_hours / config.battery_capacity_kwh;
            net_grid_w = net_w - discharge_w;
        } else {
            net_grid_w = net_w;
        }

        let bat_pct = (bat_soc / config.battery_capacity_kwh) * 100.0;
        tracker.record_step(
            &date_str,
            net_grid_w,
            r.duration_hours,
            step_cycles,
            import_price,
            export_price,
            bat_pct,
        );

        if net_grid_w > 0.0 {
            if is_demand {
                let peak = smart_peaks.entry(month_key).or_insert(0.0);
                if net_grid_w > *peak {
                    *peak = net_grid_w;
                }
            }
        }
    }
    let smart_demand = calculate_demand_charges_total(&smart_peaks, config.demand_rate);
    SimulationResultModel {
        import_kwh: tracker.import_kwh,
        export_kwh: tracker.export_kwh,
        cycles: tracker.cycles,
        energy_cost: tracker.energy_cost,
        demand_charges: smart_demand,
        net_bill: tracker.energy_cost + smart_demand,
        daily: tracker.daily,
        soc_history: tracker.soc_history,
        grid_history: tracker.grid_history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, FixedOffset};

    #[test]
    fn test_smart_negative_price_charging() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();

        // Negative import price (-5.0 cents) -> charges battery at maximum rate
        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 0.0,
            load_power_w: 1000.0,
            import_price_cents: -5.0,
            export_price_cents: -2.0,
            duration_hours: 1.0,
            day_solar_kwh: 0.0,
        }];

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 3000.0,
            min_charge_pct: 20,
            max_charge_pct: 100,
            demand_window: None,
            demand_rate: 0.0,
            negative_export_prevent: true,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods: vec![],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
            auto_cost_margin: None,
        };

        let result = run(&records, &config);

        // Import should be 1kW load + 3kW max charge rate = 4.0 kWh.
        assert_eq!(result.import_kwh, 4.0);
    }

    #[test]
    fn test_smart_high_price_feedin() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();

        // Premium export rate (65 cents >= 50 threshold) -> discharges battery for profit!
        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 0.0,
            load_power_w: 0.0,
            import_price_cents: 80.0,
            export_price_cents: 65.0,
            duration_hours: 1.0,
            day_solar_kwh: 0.0,
        }];

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 3000.0,
            min_charge_pct: 20,
            max_charge_pct: 100,
            demand_window: None,
            demand_rate: 0.0,
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: true,
            high_price_threshold: 50.0,
            periods: vec![],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
            auto_cost_margin: None,
        };

        let result = run(&records, &config);

        // Bat starts at 5kWh, reserve is 2.0 (out of demand window).
        // Discharge limit: 5.0 - 2.0 = 3.0 kWh available.
        // Maximum discharge power allows 3kW for 1h (3.0 kWh).
        // Export should be 3.0 kWh * 0.95 efficiency = 2.85 kWh.
        assert!((result.export_kwh - 2.85).abs() < 1e-9);
    }
}
