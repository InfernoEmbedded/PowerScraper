use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use chrono::{Timelike, Datelike};
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut evolved_peaks = HashMap::new();
    let mut bat_soc = config.battery_capacity_kwh * 0.5;
    let mut tracker = SimTracker::new();

    for (idx, r) in records.iter().enumerate() {
        if idx % 100 == 0 {
            std::thread::yield_now();
        }
        let month = r.dt_local.month();
        let eh_config = if let Some(ref monthly_map) = config.evolved_heuristic_monthly {
            monthly_map.get(&month.to_string()).unwrap_or(&config.evolved_heuristic)
        } else {
            &config.evolved_heuristic
        };
        let net_w = r.load_power_w - r.solar_power_w;
        let now_time = r.dt_local.time();
        let month_key = r.dt_local.format("%Y-%m").to_string();
        let date_str = r.dt_local.date_naive().to_string();
        let hour = r.dt_local.hour();
        let import_price = r.import_price_cents;
        let export_price = r.export_price_cents;

        let is_demand = config.demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end));
        let current_month_peak = *evolved_peaks.get(&month_key).unwrap_or(&0.0);

        let mut charge_w = 0.0;
        let mut discharge_w = 0.0;

        // 1. Extreme negative price: charge from grid
        if import_price < eh_config.neg_price_threshold {
            let max_avail_charge = ((config.battery_capacity_kwh * (config.max_charge_pct as f64 / 100.0) - bat_soc).max(0.0) / 0.95) / r.duration_hours * 1000.0;
            charge_w = config.max_power_w.min(max_avail_charge);
        }
        // 2. High export price: dump to grid (arbitrage)
        else if export_price >= eh_config.tier2_export_dump_threshold || export_price >= eh_config.export_dump_threshold {
            let is_near_or_in_demand = if let Some((_start, end)) = config.demand_window {
                let hour_val = hour as i32;
                let end_hour = end.hour() as i32;
                hour_val >= 12 && hour_val < end_hour
            } else {
                hour >= 12 && hour < 21
            };
            let reserve_kwh = if export_price >= eh_config.tier2_export_dump_threshold {
                eh_config.tier2_dump_reserve
            } else if is_near_or_in_demand {
                eh_config.dump_reserve_demand
            } else {
                eh_config.dump_reserve_normal
            };
            let min_pct_limit = reserve_kwh.clamp(0.0, config.battery_capacity_kwh);
            if bat_soc > min_pct_limit {
                let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = config.max_power_w.min(max_avail_discharge);
            }
        }
        // 3. Pre-charge window: top up using cheap grid
        else if {
            let is_pre_charge_window = if let Some((start, _)) = config.demand_window {
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
            let effective_soc_limit = (eh_config.pre_charge_soc_limit * (1.0 - r.day_solar_kwh * eh_config.forecast_solar_weight)).max(0.0);
            is_pre_charge_window && import_price < eh_config.pre_charge_price_threshold && (bat_soc / config.battery_capacity_kwh) < effective_soc_limit
        } {
            let effective_soc_limit = (eh_config.pre_charge_soc_limit * (1.0 - r.day_solar_kwh * eh_config.forecast_solar_weight)).max(0.0);
            let target = config.battery_capacity_kwh * effective_soc_limit;
            let deficit = (target - bat_soc).max(0.0);
            let max_avail_charge = (deficit / 0.95) / r.duration_hours * 1000.0;
            charge_w = config.max_power_w.min(max_avail_charge);
        }
        // 4. Demand window peak shaving
        else if is_demand {
            let target_peak: f64 = if eh_config.use_adaptive_shaving {
                (current_month_peak - eh_config.adaptive_safety_buffer).max(0.0f64)
            } else {
                0.0f64
            };

            if net_w > target_peak {
                let excess = net_w - target_peak;
                let max_avail_discharge = (bat_soc * 0.95) / r.duration_hours * 1000.0;
                discharge_w = excess.min(config.max_power_w).min(max_avail_discharge);
            } else if net_w < 0.0 {
                let max_avail_charge = ((config.battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            }
        }
        // 5. Standard operation
        else {
            if net_w < 0.0 {
                let max_avail_charge = ((config.battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
                charge_w = (-net_w).min(config.max_power_w).min(max_avail_charge);
            } else if net_w > 0.0 {
                let min_pct_limit = config.battery_capacity_kwh * (config.min_charge_pct as f64 / 100.0);
                let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
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
                let peak = evolved_peaks.entry(month_key).or_insert(0.0);
                if net_grid_w > *peak {
                    *peak = net_grid_w;
                }
            }
        }
    }

    let evolved_demand = calculate_demand_charges_total(&evolved_peaks, config.demand_rate);
    SimulationResultModel {
        import_kwh: tracker.import_kwh,
        export_kwh: tracker.export_kwh,
        cycles: tracker.cycles,
        energy_cost: tracker.energy_cost,
        demand_charges: evolved_demand,
        net_bill: tracker.energy_cost + evolved_demand,
        daily: tracker.daily,
        soc_history: tracker.soc_history,
        grid_history: tracker.grid_history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EvolvedHeuristicConfig;
    use chrono::{TimeZone, FixedOffset, NaiveTime};

    #[test]
    fn test_evolved_heuristic_rules() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();

        // 1. Evolved negative threshold: import_price 0.5c < evolved threshold 0.86c -> grid charging!
        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 0.0,
            load_power_w: 0.0,
            import_price_cents: 0.5,
            export_price_cents: -1.0,
            duration_hours: 1.0,
            day_solar_kwh: 0.0,
        }];

        let evolved = EvolvedHeuristicConfig {
            neg_price_threshold: 0.86,
            export_dump_threshold: 53.54,
            dump_reserve_demand: 0.0,
            dump_reserve_normal: 0.88,
            pre_charge_price_threshold: 24.97,
            pre_charge_soc_limit: 0.35,
            pre_charge_start_hour: 4,
            use_adaptive_shaving: true,
            adaptive_safety_buffer: 0.0,
            forecast_solar_weight: 0.0,
            tier2_export_dump_threshold: 1000.0,
            tier2_dump_reserve: 0.0,
        };

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
            periods: vec![],
            evolved_heuristic: evolved,
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
        };

        let result = run(&records, &config);

        // Expect charging at 3kW -> 3.0 kWh import
        assert_eq!(result.import_kwh, 3.0);
    }

    #[test]
    fn test_evolved_heuristic_extra_coverage() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap(); // Month = 6 (June)

        let mut monthly_map = HashMap::new();
        monthly_map.insert(
            "6".to_string(),
            EvolvedHeuristicConfig {
                neg_price_threshold: 0.86,
                export_dump_threshold: 10.0,
                dump_reserve_demand: 1.0,
                dump_reserve_normal: 2.0,
                pre_charge_price_threshold: 30.0,
                pre_charge_soc_limit: 0.9,
                pre_charge_start_hour: 4,
                use_adaptive_shaving: true,
                adaptive_safety_buffer: 50.0,
                forecast_solar_weight: 0.1,
                tier2_export_dump_threshold: 15.0,
                tier2_dump_reserve: 0.5,
            },
        );

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 3000.0,
            min_charge_pct: 20,
            max_charge_pct: 100,
            demand_window: Some((NaiveTime::from_hms_opt(11, 0, 0).unwrap(), NaiveTime::from_hms_opt(15, 0, 0).unwrap())),
            demand_rate: 10.0,
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods: vec![],
            evolved_heuristic: EvolvedHeuristicConfig {
                neg_price_threshold: 0.0,
                export_dump_threshold: 100.0,
                dump_reserve_demand: 0.0,
                dump_reserve_normal: 0.0,
                pre_charge_price_threshold: 0.0,
                pre_charge_soc_limit: 0.0,
                pre_charge_start_hour: 0,
                use_adaptive_shaving: false,
                adaptive_safety_buffer: 0.0,
                forecast_solar_weight: 0.0,
                tier2_export_dump_threshold: 200.0,
                tier2_dump_reserve: 0.0,
            },
            evolved_heuristic_monthly: Some(monthly_map),
            min_charge_hysteresis: None,
        };

        // 1. High export price (tier 2) -> dump reserve normal
        let records = vec![
            SimRecord {
                timestamp: dt.timestamp(),
                dt_local: dt,
                solar_power_w: 100.0,
                load_power_w: 0.0,
                import_price_cents: 5.0,
                export_price_cents: 20.0,
                duration_hours: 0.5,
                day_solar_kwh: 1.0,
            },
            // 2. Pre-charge window: month 6 config has pre_charge_start_hour = 4, hour is 12 (pre-charge window is active from 4 to 11, wait, end is start_h=11, so 12 is past. Let's do hour 8)
            SimRecord {
                timestamp: dt.timestamp(),
                dt_local: tz.with_ymd_and_hms(2026, 6, 26, 8, 0, 0).unwrap(),
                solar_power_w: 0.0,
                load_power_w: 0.0,
                import_price_cents: 15.0,
                export_price_cents: 1.0,
                duration_hours: 0.5,
                day_solar_kwh: 1.0,
            },
        ];

        let result = run(&records, &config);
        assert!(result.soc_history.len() > 0);
    }
}
