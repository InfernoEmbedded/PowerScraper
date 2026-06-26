use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut adapt_peaks = HashMap::new();
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
        let current_month_peak = *adapt_peaks.get(&month_key).unwrap_or(&0.0);

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
        } else if is_demand {
            if net_w > current_month_peak {
                let excess = net_w - current_month_peak;
                let max_avail_discharge = (bat_soc * 0.95) / r.duration_hours * 1000.0;
                discharge_w = excess.min(config.max_power_w).min(max_avail_discharge);
            } else if net_w < 0.0 {
                let max_avail_charge = ((config.battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        } else {
            if net_w > 0.0 {
                let min_pct_limit = config.battery_capacity_kwh * (config.min_charge_pct as f64 / 100.0);
                let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = net_w.min(config.max_power_w).min(max_avail_discharge);
            } else {
                let max_avail_charge = ((config.battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
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

        tracker.record_step(
            &date_str,
            net_grid_w,
            r.duration_hours,
            step_cycles,
            r.import_price_cents,
            r.export_price_cents,
        );

        if net_grid_w > 0.0 {
            if is_demand {
                let peak = adapt_peaks.entry(month_key).or_insert(0.0);
                if net_grid_w > *peak {
                    *peak = net_grid_w;
                }
            }
        }
    }
    let adapt_demand = calculate_demand_charges_total(&adapt_peaks, config.demand_rate);
    SimulationResultModel {
        import_kwh: tracker.import_kwh,
        export_kwh: tracker.export_kwh,
        cycles: tracker.cycles,
        energy_cost: tracker.energy_cost,
        demand_charges: adapt_demand,
        net_bill: tracker.energy_cost + adapt_demand,
        daily: tracker.daily,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, FixedOffset, NaiveTime};

    #[test]
    fn test_adaptive_peak_shaving() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        
        // 2 consecutive records in the demand window.
        // Record 1: load is 2000W. Peak is initially 0. Peak becomes 2000W (no battery discharge because excess is relative to peak, which starts at 0).
        // Record 2: load is 3000W. Peak is 2000W. It should discharge the battery to keep grid draw at 2000W!
        let dt1 = tz.with_ymd_and_hms(2026, 6, 26, 17, 30, 0).unwrap();
        let dt2 = tz.with_ymd_and_hms(2026, 6, 26, 18, 0, 0).unwrap();

        let records = vec![
            SimRecord {
                timestamp: dt1.timestamp(),
                dt_local: dt1,
                solar_power_w: 0.0,
                load_power_w: 2000.0,
                import_price_cents: 20.0,
                export_price_cents: 8.0,
                duration_hours: 0.5,
            },
            SimRecord {
                timestamp: dt2.timestamp(),
                dt_local: dt2,
                solar_power_w: 0.0,
                load_power_w: 3000.0, // 3000W exceeds monthly peak of 2000W
                import_price_cents: 20.0,
                export_price_cents: 8.0,
                duration_hours: 0.5,
            }
        ];

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 3000.0,
            min_charge_pct: 20,
            max_charge_pct: 100,
            demand_window: Some((NaiveTime::from_hms_opt(17, 0, 0).unwrap(), NaiveTime::from_hms_opt(20, 0, 0).unwrap())),
            demand_rate: 0.15,
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods: vec![],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
        };

        let result = run(&records, &config);

        // For Record 1, peak starts at 0, battery has charge, discharges 2000W to keep grid draw at 0W.
        // For Record 2, peak is still 0, battery discharges 3000W to keep grid draw at 0W.
        // Total import: 0.0 kWh. Monthly peak draw should be exactly 0W.
        assert_eq!(result.import_kwh, 0.0);
        assert_eq!(result.demand_charges, 0.0);
    }
}
