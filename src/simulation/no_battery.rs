use super::{SimRecord, SimConfig, SimTracker, is_time_in_window, calculate_demand_charges_total};
use crate::power_manager::SimulationResultModel;
use std::collections::HashMap;

pub fn run(records: &[SimRecord], config: &SimConfig) -> SimulationResultModel {
    let mut no_bat_peaks = HashMap::new();
    let mut tracker = SimTracker::new();

    for r in records {
        let net_w = r.load_power_w - r.solar_power_w;
        let now_time = r.dt_local.time();
        let month_key = r.dt_local.format("%Y-%m").to_string();
        let date_str = r.dt_local.date_naive().to_string();

        tracker.record_step(
            &date_str,
            net_w,
            r.duration_hours,
            0.0,
            r.import_price_cents,
            r.export_price_cents,
            0.0,
        );

        if net_w > 0.0 {
            if config.demand_window.map_or(false, |(start, end)| is_time_in_window(now_time, start, end)) {
                let peak = no_bat_peaks.entry(month_key).or_insert(0.0);
                if net_w > *peak {
                    *peak = net_w;
                }
            }
        }
    }
    let no_bat_demand = calculate_demand_charges_total(&no_bat_peaks, config.demand_rate);
    SimulationResultModel {
        import_kwh: tracker.import_kwh,
        export_kwh: tracker.export_kwh,
        cycles: tracker.cycles,
        energy_cost: tracker.energy_cost,
        demand_charges: no_bat_demand,
        net_bill: tracker.energy_cost + no_bat_demand,
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
    fn test_no_battery_simulation() {
        let tz = FixedOffset::east_opt(36000).unwrap(); // AEST
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();

        // 1 hour record, 2000W load, 500W solar
        // net_w = 1500W (importing 1.5 kWh)
        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 500.0,
            load_power_w: 2000.0,
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
            demand_window: Some((NaiveTime::from_hms_opt(17, 0, 0).unwrap(), NaiveTime::from_hms_opt(20, 0, 0).unwrap())),
            demand_rate: 0.15,
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods: vec![],
            evolved_heuristic: crate::config::EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
        };

        let result = run(&records, &config);

        // Without battery, net load is positive, import should be 1.5 kWh.
        assert_eq!(result.import_kwh, 1.5);
        assert_eq!(result.export_kwh, 0.0);
        assert_eq!(result.cycles, 0.0);
        assert_eq!(result.energy_cost, 1.5 * 0.20); // 1.5 kWh @ 20 cents
        assert_eq!(result.demand_charges, 0.0); // Out of demand window (12:00)
    }

    #[test]
    fn test_no_battery_demand_window() {
        let tz = FixedOffset::east_opt(36000).unwrap(); // AEST
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 18, 0, 0).unwrap(); // Inside 17:00-20:00

        let records = vec![SimRecord {
            timestamp: dt.timestamp(),
            dt_local: dt,
            solar_power_w: 0.0,
            load_power_w: 3000.0, // 3kW draw
            import_price_cents: 30.0,
            export_price_cents: 10.0,
            duration_hours: 0.5, // 30 mins
            day_solar_kwh: 0.0,
        }];

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
            evolved_heuristic_monthly: None,
            min_charge_hysteresis: None,
        };

        let result = run(&records, &config);

        assert_eq!(result.import_kwh, 1.5); // 3kW * 0.5h
        // Peak is 3kW (3000W)
        // Demand charge: 3 kW * 0.15 $/kW/day * 30 days = 13.5
        assert!((result.demand_charges - 13.5).abs() < 1e-9);
    }
}
