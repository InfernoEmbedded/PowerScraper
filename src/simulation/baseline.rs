use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut base_peaks = HashMap::new();
    let mut bat_soc = config.battery_capacity_kwh * 0.5;
    let mut tracker = SimTracker::new();

    for r in records {
        let net_w = r.load_power_w - r.solar_power_w;
        let now_time = r.dt_local.time();
        let month_key = r.dt_local.format("%Y-%m").to_string();
        let date_str = r.dt_local.date_naive().to_string();

        let net_grid_w;
        let step_cycles;
        if net_w > 0.0 {
            let max_avail_discharge = (bat_soc * 0.95) / r.duration_hours * 1000.0;
            let discharge = net_w.min(config.max_power_w).min(max_avail_discharge);

            bat_soc -= (discharge / 1000.0) * r.duration_hours / 0.95;
            step_cycles = (discharge / 1000.0) * r.duration_hours / config.battery_capacity_kwh;
            net_grid_w = net_w - discharge;
        } else {
            let max_avail_charge = ((config.battery_capacity_kwh - bat_soc) / 0.95) / r.duration_hours * 1000.0;
            let charge = (-net_w).min(config.max_power_w).min(max_avail_charge);

            bat_soc += (charge / 1000.0) * r.duration_hours * 0.95;
            step_cycles = (charge / 1000.0) * r.duration_hours / config.battery_capacity_kwh;
            net_grid_w = net_w + charge;
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
                let peak = base_peaks.entry(month_key).or_insert(0.0);
                if net_grid_w > *peak {
                    *peak = net_grid_w;
                }
            }
        }
    }
    let base_demand = calculate_demand_charges_total(&base_peaks, config.demand_rate);
    SimulationResultModel {
        import_kwh: tracker.import_kwh,
        export_kwh: tracker.export_kwh,
        cycles: tracker.cycles,
        energy_cost: tracker.energy_cost,
        demand_charges: base_demand,
        net_bill: tracker.energy_cost + base_demand,
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
    fn test_baseline_solar_charging() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();

        // Excess solar of 3000W for 1 hour.
        // bat_soc starts at 5kWh (50% of 10kWh)
        // With 95% charge efficiency, it should charge: 3.0 kW * 1h * 0.95 = 2.85 kWh
        // New SOC should be 5 + 2.85 = 7.85 kWh
        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 3000.0,
            load_power_w: 0.0,
            import_price_cents: 20.0,
            export_price_cents: 8.0,
            duration_hours: 1.0,
            day_solar_kwh: 0.0,
        }];

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 5000.0,
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
            auto_cost_margin: None,
        };

        let result = run(&records, &config);

        // Since solar excess charges the battery, grid import/export should remain 0
        assert_eq!(result.import_kwh, 0.0);
        assert_eq!(result.export_kwh, 0.0);
        assert_eq!(result.cycles, 3.0 / 10.0); // 0.3 cycles
    }

    #[test]
    fn test_baseline_discharging() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 20, 0, 0).unwrap();

        // 3000W load, no solar.
        // bat_soc starts at 5kWh.
        // Max discharge available (95% efficiency): 5.0 * 0.95 = 4.75 kWh -> 4750W for 1h.
        // 3000W load for 1 hour requires 3kWh of discharge.
        // Battery discharge: 3.0 / 0.95 = 3.158 kWh.
        // Grid import should be 0.
        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 0.0,
            load_power_w: 3000.0,
            import_price_cents: 20.0,
            export_price_cents: 8.0,
            duration_hours: 1.0,
            day_solar_kwh: 0.0,
        }];

        let config = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 5000.0,
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
            auto_cost_margin: None,
        };

        let result = run(&records, &config);

        assert_eq!(result.import_kwh, 0.0);
        assert_eq!(result.export_kwh, 0.0);
        assert_eq!(result.cycles, 3.0 / 10.0);
    }
}
