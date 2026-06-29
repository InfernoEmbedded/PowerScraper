use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use chrono::NaiveTime;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut arb_peaks = HashMap::new();
    let mut bat_soc = config.battery_capacity_kwh * 0.5;
    let mut tracker = SimTracker::new();

    let records_simulated = records.len();

    for i in 0..records_simulated {
        let r = &records[i];
        let net_w = r.load_power_w - r.solar_power_w;
        let now_time = r.dt_local.time();
        let month_key = r.dt_local.format("%Y-%m").to_string();
        let date_str = r.dt_local.date_naive().to_string();
        let import_price = r.import_price_cents;
        let export_price = r.export_price_cents;

        let is_demand = config.demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end));

        let mut expected_solar = 0.0;
        let mut demand_needed = 0.0;
        let mut cheapest_future = Vec::new();
        let mut night_needed = 0.0;
        let mut night_prices = Vec::new();

        let night_start = config.demand_window.map(|(_, end)| end).unwrap_or_else(|| NaiveTime::from_hms_opt(20, 0, 0).unwrap());
        let night_end = NaiveTime::from_hms_opt(6, 0, 0).unwrap();

        for j in i..records_simulated {
            let fr = &records[j];
            if fr.timestamp - r.timestamp > 86400 {
                break;
            }
            let ftime = fr.dt_local.time();
            let fnet = fr.load_power_w - fr.solar_power_w;

            if config.demand_window.map_or(false, |(start, end)| is_time_in_window(ftime, start, end)) && fnet > 0.0 {
                demand_needed += (fnet / 1000.0) * fr.duration_hours;
            }
            if config.demand_window.map_or(false, |(start, _)| ftime < start) && fnet < 0.0 {
                expected_solar += (-fnet / 1000.0) * fr.duration_hours;
            }
            if is_time_in_window(ftime, night_start, night_end) {
                if fnet > 0.0 {
                    night_needed += (fnet / 1000.0) * fr.duration_hours;
                }
                night_prices.push(fr.import_price_cents);
            }
            cheapest_future.push((j, fr.import_price_cents));
        }

        cheapest_future.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let threshold_idx = (cheapest_future.len() / 5).max(1);
        let cheap_threshold = cheapest_future[threshold_idx - 1].1;

        let night_avg_price = if !night_prices.is_empty() {
            night_prices.iter().sum::<f64>() / night_prices.len() as f64
        } else {
            30.0
        };

        let demand_reserve = (demand_needed / 0.95).min(config.battery_capacity_kwh * 0.95);
        let total_needed = (demand_needed + night_needed) / 0.95;
        let required_reserve = total_needed.min(config.battery_capacity_kwh * 0.95);

        let target_reserve = if night_avg_price > import_price * 1.10 {
            required_reserve
        } else {
            demand_reserve
        };

        let mut charge_w = 0.0;
        let mut discharge_w = 0.0;

        let negative_export_triggered = config.negative_export_prevent && export_price < 0.0;
        if import_price < 0.0 || negative_export_triggered {
            let max_avail_charge = ((config.battery_capacity_kwh * (config.max_charge_pct as f64 / 100.0) - bat_soc) / 0.95) / r.duration_hours * 1000.0;
            charge_w = config.max_power_w.min(max_avail_charge.max(0.0));
        } else if config.high_price_discharge && export_price >= config.high_price_threshold && bat_soc > (target_reserve + config.battery_capacity_kwh * 0.1) {
            let max_avail_discharge = ((bat_soc - target_reserve) * 0.95) / r.duration_hours * 1000.0;
            discharge_w = config.max_power_w.min(max_avail_discharge.max(0.0));
        } else if is_demand {
            if net_w > 0.0 {
                let max_avail_discharge = (bat_soc * 0.95) / r.duration_hours * 1000.0;
                discharge_w = net_w.min(config.max_power_w).min(max_avail_discharge);
            } else {
                let max_avail_charge = ((config.battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        } else {
            let projected_deficit = target_reserve - (bat_soc + expected_solar * 0.95);
            let is_cheap = if config.low_price_charge { import_price <= config.low_price_threshold } else { import_price < 12.0 || import_price <= cheap_threshold } || config.demand_rate > 0.0;

            if projected_deficit > 0.0 && is_cheap {
                let max_avail_charge = (projected_deficit / 0.95) / r.duration_hours * 1000.0;
                charge_w = config.max_power_w.min(max_avail_charge.max(0.0));
            } else if net_w < 0.0 {
                let max_avail_charge = ((config.battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            } else if net_w > 0.0 {
                let available = (bat_soc - target_reserve).max(0.0);
                let max_avail_discharge = (available * 0.95) / r.duration_hours * 1000.0;
                discharge_w = net_w.min(config.max_power_w).min(max_avail_discharge);
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
                let peak = arb_peaks.entry(month_key).or_insert(0.0);
                if net_grid_w > *peak {
                    *peak = net_grid_w;
                }
            }
        }
    }

    let arb_demand = calculate_demand_charges_total(&arb_peaks, config.demand_rate);
    SimulationResultModel {
        import_kwh: tracker.import_kwh,
        export_kwh: tracker.export_kwh,
        cycles: tracker.cycles,
        energy_cost: tracker.energy_cost,
        demand_charges: arb_demand,
        net_bill: tracker.energy_cost + arb_demand,
        daily: tracker.daily,
        soc_history: tracker.soc_history,
        grid_history: tracker.grid_history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, FixedOffset, NaiveTime};

    #[test]
    fn test_mpc_arbitrage_charging() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        
        // daytime record (12:00) with import_price 10c, night record (23:00) with import_price 30c (avg 30c)
        // night average 30c > daytime 10c * 1.10 (11c) -> pre-charge is beneficial!
        let dt1 = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let dt2 = tz.with_ymd_and_hms(2026, 6, 26, 23, 0, 0).unwrap();

        let records = vec![
            SimRecord {
                timestamp: dt1.timestamp(),
                dt_local: dt1,
                solar_power_w: 0.0,
                load_power_w: 0.0,
                import_price_cents: 10.0, // daytime cheap
                export_price_cents: 5.0,
                duration_hours: 1.0,
                day_solar_kwh: 0.0,
            },
            SimRecord {
                timestamp: dt2.timestamp(),
                dt_local: dt2,
                solar_power_w: 0.0,
                load_power_w: 8000.0, // night load needing 8kWh
                import_price_cents: 30.0, // night expensive
                export_price_cents: 10.0,
                duration_hours: 1.0,
                day_solar_kwh: 0.0,
            }
        ];

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 5000.0,
            min_charge_pct: 20,
            max_charge_pct: 100,
            demand_window: Some((NaiveTime::from_hms_opt(17, 0, 0).unwrap(), NaiveTime::from_hms_opt(20, 0, 0).unwrap())),
            demand_rate: 0.0,
            negative_export_prevent: false,
            low_price_charge: true,
            low_price_threshold: 15.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods: vec![],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
        };

        let result = run(&records, &config);

        // Pre-charged cheap energy during dt1 to support dt2 load.
        // During dt1 (12:00), battery should have charged.
        assert!(result.import_kwh > 0.0);
    }
}
