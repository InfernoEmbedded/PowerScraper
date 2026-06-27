use crate::config::BatteryControlInverter;
use crate::power_manager::{InverterState, get_timezone_offset};
use std::collections::HashMap;

pub fn calculate_power_budget(
    total_solar_production: f64,
    total_consumption: f64,
) -> f64 {
    total_solar_production - total_consumption
}

pub fn calculate_power_budget_with_charging(
    db_path: &str,
    timezone: Option<&str>,
    total_consumption: f64,
    inverters_config: &HashMap<String, BatteryControlInverter>,
    inverters_state: &HashMap<String, InverterState>,
) -> f64 {
    let tz_offset = get_timezone_offset(timezone);
    let now = chrono::Utc::now().with_timezone(&tz_offset);

    // Power Budget with Charging Calculations
    let end_of_today = now.date_naive()
        .and_time(chrono::NaiveTime::from_hms_opt(23, 59, 59).unwrap())
        .and_local_timezone(tz_offset)
        .single()
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| now.timestamp() + 86400);

    let mut last_solar_ts = now.timestamp();
    let mut expected_solar_wh = 0.0;

    if let Ok(conn) = rusqlite::Connection::open(db_path) {
        let now_ts = now.timestamp();
        if let Ok(mut stmt) = conn.prepare("SELECT timestamp, predicted_solar_w FROM solar_forecast WHERE timestamp >= ?1 AND timestamp <= ?2 ORDER BY timestamp ASC") {
            if let Ok(mut rows) = stmt.query(rusqlite::params![now_ts, end_of_today]) {
                let mut prev_ts = None;
                while let Ok(Some(row)) = rows.next() {
                    let ts: i64 = row.get(0).unwrap_or(0);
                    let val: f64 = row.get(1).unwrap_or(0.0);
                    if val > 0.0 {
                        last_solar_ts = last_solar_ts.max(ts);
                    }
                    if let Some(pts) = prev_ts {
                        let diff_hours = (ts - pts) as f64 / 3600.0;
                        if diff_hours > 0.0 && diff_hours <= 2.0 {
                            expected_solar_wh += val * diff_hours;
                        }
                    } else {
                        let diff_hours = (ts - now_ts) as f64 / 3600.0;
                        if diff_hours > 0.0 && diff_hours <= 2.0 {
                            expected_solar_wh += val * diff_hours;
                        }
                    }
                    prev_ts = Some(ts);
                }
            }
        }
    }

    let now_ts = now.timestamp();
    let daylight_remaining_hours = (last_solar_ts - now_ts).max(0) as f64 / 3600.0;

    let mut charge_needed_wh = 0.0;
    for (name, inv_cfg) in inverters_config {
        let state = inverters_state.get(name).cloned().unwrap_or_default();
        let soc = state.battery_capacity as f64;
        let cap_kwh = inv_cfg.battery_capacity.unwrap_or(13.8);
        let calc_cap = if let Ok(status) = crate::web_server::get_system_status().lock() {
            status.inverters.get(name).and_then(|i| i.calculated_battery_capacity)
        } else {
            None
        };
        let cap_wh = calc_cap.unwrap_or(cap_kwh) * 1000.0;
        let max_soc = inv_cfg.max_charge_pct.unwrap_or(95) as f64;
        if soc < max_soc {
            charge_needed_wh += cap_wh * (max_soc - soc) / 100.0;
        }
    }
    let solar_needed_for_charging_wh = charge_needed_wh / 0.95;

    if daylight_remaining_hours > 0.0 {
        let available_solar_wh = (expected_solar_wh - solar_needed_for_charging_wh).max(0.0);
        (available_solar_wh / daylight_remaining_hours - total_consumption).max(0.0)
    } else {
        0.0
    }
}
