use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use chrono::Timelike;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut evolved_peaks = HashMap::new();
    let mut bat_soc = config.battery_capacity_kwh * 0.5;
    let mut tracker = SimTracker::new();

    let eh_config = &config.evolved_heuristic;

    for r in records {
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
        else if export_price >= eh_config.export_dump_threshold {
            let is_near_or_in_demand = if let Some((_start, end)) = config.demand_window {
                let hour_val = hour as i32;
                let end_hour = end.hour() as i32;
                hour_val >= 12 && hour_val < end_hour
            } else {
                hour >= 12 && hour < 21
            };
            let reserve_kwh = if is_near_or_in_demand {
                eh_config.dump_reserve_demand
            } else {
                eh_config.dump_reserve_normal
            };
            let reserve_pct = ((reserve_kwh / config.battery_capacity_kwh) * 100.0) as u8;
            let min_pct_limit = config.battery_capacity_kwh * (reserve_pct as f64 / 100.0);
            if bat_soc > min_pct_limit {
                let max_avail_discharge = ((bat_soc - min_pct_limit).max(0.0) * 0.95) / r.duration_hours * 1000.0;
                discharge_w = config.max_power_w.min(max_avail_discharge);
            }
        }
        // 3. Pre-charge window: top up using cheap grid
        else if {
            let is_pre_charge_window = if let Some((start, _)) = config.demand_window {
                hour >= eh_config.pre_charge_start_hour && hour < start.hour()
            } else {
                hour >= eh_config.pre_charge_start_hour && hour < 15
            };
            is_pre_charge_window && import_price < eh_config.pre_charge_price_threshold && (bat_soc / config.battery_capacity_kwh) < eh_config.pre_charge_soc_limit
        } {
            let target = config.battery_capacity_kwh * eh_config.pre_charge_soc_limit;
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

        tracker.record_step(
            &date_str,
            net_grid_w,
            r.duration_hours,
            step_cycles,
            import_price,
            export_price,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EvolvedHeuristicConfig;
    use chrono::{TimeZone, FixedOffset};

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
        };

        let result = run(&records, &config);

        // Expect charging at 3kW -> 3.0 kWh import
        assert_eq!(result.import_kwh, 3.0);
    }
}
