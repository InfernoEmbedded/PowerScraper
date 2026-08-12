use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use chrono::NaiveTime;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut auto_peaks = HashMap::new();
    let mut bat_soc = config.battery_capacity_kwh * 0.5;
    let initial_export = records.first().map_or(0.0, |r| r.export_price_cents);
    let mut bat_total_cost = bat_soc * initial_export;
    let mut tracker = SimTracker::new();
    let mut is_low_capacity = false;

    for r in records {
        let net_w = r.load_power_w - r.solar_power_w;
        let month_key = r.dt_local.format("%Y-%m").to_string();
        let date_str = r.dt_local.date_naive().to_string();

        let mut charge_w = 0.0;
        let mut discharge_w = 0.0;

        let now_time = r.dt_local.time();

        let bat_unit_cost = if bat_soc > 1e-6 {
            bat_total_cost / bat_soc
        } else {
            r.export_price_cents
        };

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

        let min_pct = active_period.map(|p| p.min_charge.max(config.min_charge_pct)).unwrap_or(config.min_charge_pct) as f64;
        let grid_charge = active_period.map(|p| p.grid_charge).unwrap_or(false);
        let prefer_battery = active_period.map(|p| p.prefer_battery).unwrap_or(false);
        let force_discharge = active_period.and_then(|p| p.force_discharge);

        let bat_pct = (bat_soc / config.battery_capacity_kwh) * 100.0;
        let hyst = active_period.and_then(|p| p.min_charge_hysteresis).or(config.min_charge_hysteresis).unwrap_or(3) as f64;
        is_low_capacity = if is_low_capacity {
            bat_pct < min_pct + hyst
        } else {
            bat_pct < min_pct
        };

        if let Some(fd_w) = force_discharge {
            if fd_w > 0.0 {
                let max_avail_discharge = ((bat_soc - (min_pct / 100.0) * config.battery_capacity_kwh).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = fd_w.min(config.max_power_w).min(max_avail_discharge);
            } else if fd_w < 0.0 {
                let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-fd_w).min(config.max_power_w).min(max_avail_charge);
            }
        } else if grid_charge && is_low_capacity {
            let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
            charge_w = config.max_power_w.min(max_avail_charge);
        } else if prefer_battery && is_low_capacity {
            if net_w < 0.0 {
                let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        } else {
            if net_w > 0.0 {
                let allow_discharge = match config.auto_cost_margin {
                    Some(margin) => (bat_unit_cost + margin) <= r.import_price_cents,
                    None => true,
                };
                if allow_discharge {
                    let max_avail_discharge = ((bat_soc - (min_pct / 100.0) * config.battery_capacity_kwh).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                    discharge_w = net_w.min(config.max_power_w).min(max_avail_discharge);
                }
            } else {
                let max_avail_charge = (((config.battery_capacity_kwh * 0.95) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        }

        let net_grid_w;
        let mut step_cycles = 0.0;
        if charge_w > 0.0 {
            let charge_kwh = (charge_w / 1000.0) * r.duration_hours * 0.95;
            bat_soc += charge_kwh;
            step_cycles = (charge_w / 1000.0) * r.duration_hours / config.battery_capacity_kwh;
            net_grid_w = net_w + charge_w;

            let excess_solar_w = (r.solar_power_w - r.load_power_w).max(0.0);
            let solar_charge_w = charge_w.min(excess_solar_w);
            let grid_charge_w = charge_w - solar_charge_w;

            let solar_kwh = (solar_charge_w / 1000.0) * r.duration_hours * 0.95;
            let grid_kwh = (grid_charge_w / 1000.0) * r.duration_hours * 0.95;

            let added_cost = solar_kwh * r.export_price_cents + grid_kwh * r.import_price_cents;
            bat_total_cost += added_cost;
        } else if discharge_w > 0.0 {
            let discharge_kwh_removed = (discharge_w / 1000.0) * r.duration_hours / 0.95;
            let removed_cost = discharge_kwh_removed * bat_unit_cost;
            bat_soc -= discharge_kwh_removed;
            bat_total_cost = (bat_total_cost - removed_cost).max(0.0);
            if bat_soc <= 1e-6 {
                bat_soc = 0.0;
                bat_total_cost = 0.0;
            }
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
            min_charge_hysteresis: None,
            ignore_cost_margin: false,
        }];

        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 0.0,
            load_power_w: 1000.0, // 1kW home load
            import_price_cents: 20.0,
            export_price_cents: 8.0,
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
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods,
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
            auto_cost_margin: None,
        };

        let result = run(&records, &config);
        assert_eq!(result.import_kwh, 4.0);
    }

    #[test]
    fn test_auto_simulation_hysteresis() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let base_dt = tz.with_ymd_and_hms(2026, 6, 26, 2, 0, 0).unwrap(); // 02:00 AM

        let mut records = Vec::new();
        for i in 0..4 {
            records.push(SimRecord {
                timestamp: base_dt.timestamp() + i * 3600,
                dt_local: base_dt + chrono::Duration::hours(i),
                solar_power_w: 0.0,
                load_power_w: 0.0,
                import_price_cents: 20.0,
                export_price_cents: 8.0,
                duration_hours: 1.0,
                day_solar_kwh: 0.0,
            });
        }

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 1000.0,
            min_charge_pct: 20,
            max_charge_pct: 100,
            demand_window: None,
            demand_rate: 0.0,
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods: vec![BatteryControlPeriod {
                start: "01:00:00".to_string(),
                end: "10:00:00".to_string(),
                min_charge: 60,
                grid_charge: true,
                force_discharge: None,
                grace: false,
                prefer_battery: false,
                min_charge_hysteresis: Some(10),
                ignore_cost_margin: false,
            }],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
            auto_cost_margin: None,
        };

        let result = run(&records, &config);
        assert_eq!(result.import_kwh, 3.0);
    }

    #[test]
    fn test_auto_cost_margin_discharge_allowed_and_suppressed() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt1 = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let dt2 = tz.with_ymd_and_hms(2026, 6, 26, 13, 0, 0).unwrap();

        // Step 1: Solar charges battery (initial export 8.0 c/kWh).
        // Step 2: Load of 1000W. Import price 20.0 c/kWh. Margin 2.0.
        // Battery unit cost (8.0) + margin (2.0) = 10.0 <= 20.0 -> Discharge allowed!
        let records_allowed = vec![SimRecord {
            timestamp: dt1.timestamp(),
            dt_local: dt1,
            solar_power_w: 0.0,
            load_power_w: 1000.0,
            import_price_cents: 20.0,
            export_price_cents: 8.0,
            duration_hours: 1.0,
            day_solar_kwh: 0.0,
        }];

        let config_allowed = SimConfig {
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
            periods: vec![],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
            auto_cost_margin: Some(2.0),
        };

        let res_allowed = run(&records_allowed, &config_allowed);
        assert_eq!(res_allowed.import_kwh, 0.0);

        // Step 3: Battery charged from expensive grid during step 1 (import 30 c/kWh).
        // Step 2: Load 1000W, import price drops to 15 c/kWh. Margin 2.0.
        // Battery unit cost (30.0) + margin (2.0) = 32.0 > 15.0 -> Discharge suppressed!
        let records_suppressed = vec![
            SimRecord {
                timestamp: dt1.timestamp(),
                dt_local: dt1,
                solar_power_w: 0.0,
                load_power_w: 0.0,
                import_price_cents: 30.0,
                export_price_cents: 8.0,
                duration_hours: 1.0,
                day_solar_kwh: 0.0,
            },
            SimRecord {
                timestamp: dt2.timestamp(),
                dt_local: dt2,
                solar_power_w: 0.0,
                load_power_w: 1000.0,
                import_price_cents: 15.0,
                export_price_cents: 8.0,
                duration_hours: 1.0,
                day_solar_kwh: 0.0,
            },
        ];

        let config_suppressed = SimConfig {
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
            periods: vec![BatteryControlPeriod {
                start: "11:30:00".to_string(),
                end: "12:30:00".to_string(),
                min_charge: 80,
                grid_charge: true,
                force_discharge: None,
                grace: false,
                prefer_battery: false,
                min_charge_hysteresis: None,
                ignore_cost_margin: false,
            }],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
            auto_cost_margin: Some(2.0),
        };

        let res_suppressed = run(&records_suppressed, &config_suppressed);
        // During dt2, discharge was suppressed because 30+2 > 15, so import_kwh for 1kW load is 1.0 kWh.
        // Plus grid charge import in dt1 (3.0 kWh) = 4.0 kWh total import.
        assert_eq!(res_suppressed.import_kwh, 4.0);
    }

    #[test]
    fn test_auto_simulation_solar_and_grid_cost_apportionment() {
        let dt1 = chrono::DateTime::parse_from_rfc3339("2026-08-10T12:00:00+10:00").unwrap();

        let records = vec![
            SimRecord {
                timestamp: dt1.timestamp(),
                dt_local: dt1,
                solar_power_w: 3000.0,
                load_power_w: 1000.0,
                import_price_cents: 30.0,
                export_price_cents: 10.0,
                duration_hours: 1.0,
                day_solar_kwh: 3.0,
            },
        ];

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
            periods: vec![BatteryControlPeriod {
                start: "11:30:00".to_string(),
                end: "12:30:00".to_string(),
                min_charge: 80,
                grid_charge: true,
                force_discharge: None,
                grace: false,
                prefer_battery: false,
                min_charge_hysteresis: None,
                ignore_cost_margin: false,
            }],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
            auto_cost_margin: None,
        };

        let res = run(&records, &config);
        assert_eq!(res.import_kwh, 1.0);
    }
}
