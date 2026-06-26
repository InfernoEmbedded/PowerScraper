use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use chrono::NaiveTime;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut auto_peaks = HashMap::new();
    let mut bat_soc = config.battery_capacity_kwh * 0.5;
    let mut tracker = SimTracker::new();

    for r in records {
        let net_w = r.load_power_w - r.solar_power_w;
        let month_key = r.dt_local.format("%Y-%m").to_string();
        let date_str = r.dt_local.date_naive().to_string();

        let mut charge_w = 0.0;
        let mut discharge_w = 0.0;

        let now_time = r.dt_local.time();

        // Match period
        let mut active_period = None;
        for period in &config.periods {
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

        let min_pct = active_period.map(|p| p.min_charge).unwrap_or(config.min_charge_pct) as f64;
        let grid_charge = active_period.map(|p| p.grid_charge).unwrap_or(false);
        let prefer_battery = active_period.map(|p| p.prefer_battery).unwrap_or(false);
        let force_discharge = active_period.and_then(|p| p.force_discharge);

        let bat_pct = (bat_soc / config.battery_capacity_kwh) * 100.0;

        if let Some(fd_w) = force_discharge {
            if fd_w > 0.0 {
                let max_avail_discharge = ((bat_soc - (min_pct / 100.0) * config.battery_capacity_kwh).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = fd_w.min(config.max_power_w).min(max_avail_discharge);
            } else if fd_w < 0.0 {
                let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-fd_w).min(config.max_power_w).min(max_avail_charge);
            }
        } else if grid_charge && bat_pct < min_pct {
            let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
            charge_w = config.max_power_w.min(max_avail_charge);
        } else if prefer_battery && bat_pct < min_pct {
            if net_w < 0.0 {
                let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        } else {
            if net_w > 0.0 {
                let max_avail_discharge = ((bat_soc - (min_pct / 100.0) * config.battery_capacity_kwh).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = net_w.min(config.max_power_w).min(max_avail_discharge);
            } else {
                let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
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
            r.import_price_cents,
            r.export_price_cents,
            bat_pct,
        );

        if net_grid_w > 0.0 {
            if config.demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                let peak = auto_peaks.entry(month_key).or_insert(0.0);
                if net_grid_w > *peak {
                    *peak = net_grid_w;
                }
            }
        }
    }
    let auto_demand = calculate_demand_charges_total(&auto_peaks, config.demand_rate);
    SimulationResultModel {
        import_kwh: tracker.import_kwh,
        export_kwh: tracker.export_kwh,
        cycles: tracker.cycles,
        energy_cost: tracker.energy_cost,
        demand_charges: auto_demand,
        net_bill: tracker.energy_cost + auto_demand,
        daily: tracker.daily,
        soc_history: tracker.soc_history,
        grid_history: tracker.grid_history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BatteryControlPeriod;
    use chrono::{TimeZone, FixedOffset};

    #[test]
    fn test_auto_grid_charge_period() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 2, 0, 0).unwrap(); // 02:00 AM

        // bat_soc is 5kWh (50%), target min_charge is 80% (8kWh)
        // With grid_charge: true, it should charge from grid!
        let periods = vec![BatteryControlPeriod {
            start: "01:00:00".to_string(),
            end: "05:00:00".to_string(),
            min_charge: 80,
            grid_charge: true,
            force_discharge: None,
            grace: false,
            prefer_battery: false,
        }];

        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 0.0,
            load_power_w: 1000.0, // 1kW home load
            import_price_cents: 20.0,
            export_price_cents: 8.0,
            duration_hours: 1.0,
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
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods,
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
        };

        let result = run(&records, &config);

        // Grid charge rate should be max power W (3000W) or available deficit (3000W)
        // Since load is 1000W and battery charges at 3000W, total grid import is 4000W -> 4.0 kWh.
        assert_eq!(result.import_kwh, 4.0);
    }
}
