use axum::Json;
use std::collections::HashMap;

#[derive(serde::Deserialize)]
pub struct InferRequest {
    pub inverter: String,
    pub string: String,
    pub latitude: f64,
    pub longitude: f64,
}

pub async fn handle_infer_orientation(
    db_path: String,
    Json(payload): Json<InferRequest>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let topic = format!("{}/{} Power", payload.inverter, payload.string);
    
    let one_year_ago = chrono::Utc::now().timestamp() - (365 * 24 * 3600);
    let records = crate::database::get_telemetry_history(&db_path, &topic, one_year_ago)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?;
    
    let mut daily_data: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
    for (ts, val) in records {
        let day = ts / 86400;
        daily_data.entry(day).or_default().push((ts, val));
    }
    
    if daily_data.is_empty() {
        return Err((axum::http::StatusCode::BAD_REQUEST, "No historical solar telemetry found for this inverter and PV string.".to_string()));
    }
    
    let mut summer_days = Vec::new();
    let mut winter_days = Vec::new();
    
    for (&day, pts) in &daily_data {
        let daytime_pts: Vec<f64> = pts.iter()
            .filter(|&&(_, v)| v > 10.0)
            .map(|&(_, v)| v)
            .collect();
        if daytime_pts.len() >= 5 {
            let sum: f64 = daytime_pts.iter().sum();
            let avg = sum / daytime_pts.len() as f64;
            
            if let Some(&(ts, _)) = pts.first() {
                if let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) {
                    use chrono::Datelike;
                    let month = dt.month();
                    // Southern hemisphere summer: Oct (10) to Mar (3)
                    if month >= 10 || month <= 3 {
                        summer_days.push((day, avg));
                    } else {
                        winter_days.push((day, avg));
                    }
                } else {
                    winter_days.push((day, avg));
                }
            }
        }
    }
    
    summer_days.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    winter_days.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    
    let mut top_days: Vec<i64> = summer_days.iter().take(3).map(|&(day, _)| day).collect();
    for &(day, _) in winter_days.iter().take(3) {
        top_days.push(day);
    }
    
    if top_days.is_empty() {
        let mut all_days = Vec::new();
        all_days.extend(summer_days);
        all_days.extend(winter_days);
        all_days.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        top_days = all_days.iter().take(5).map(|&(day, _)| day).collect();
    }
    
    let mut points = Vec::new();
    for day in top_days {
        if let Some(pts) = daily_data.get(&day) {
            for &(ts, val) in pts {
                if val > 10.0 {
                    points.push((ts, val));
                }
            }
        }
    }
    
    if points.len() < 24 {
        return Err((axum::http::StatusCode::BAD_REQUEST, "Insufficient daytime telemetry points found for clear-sky days to run inference.".to_string()));
    }
    
    let mut best_r = -2.0;
    let mut best_tilt = 20.0;
    let mut best_azimuth = 180.0;
    
    let mut precomputed_solar = Vec::with_capacity(points.len());
    for &(ts, actual_w) in &points {
        let utc_time = chrono::DateTime::from_timestamp(ts, 0)
            .unwrap_or_else(|| chrono::Utc::now());
        let (el, az) = crate::power_manager::calculate_solar_position(payload.latitude, payload.longitude, utc_time);
        precomputed_solar.push((actual_w, el, az));
    }
    
    for t_deg in (0..=60).step_by(2) {
        for a_deg in (0..360).step_by(5) {
            let t = t_deg as f64;
            let a = a_deg as f64;
            
            let mut sum_x = 0.0;
            let mut sum_y = 0.0;
            let mut sum_x2 = 0.0;
            let mut sum_y2 = 0.0;
            let mut sum_xy = 0.0;
            let n = precomputed_solar.len() as f64;
            
            for &(actual_w, el, az) in &precomputed_solar {
                let dni = 900.0 * el.sin().max(0.0);
                let dhi = 120.0 * el.sin().max(0.0);
                let modeled_poa = crate::power_manager::calculate_poa_irradiance(dni, dhi, el, az, t, a);
                sum_x += actual_w;
                sum_y += modeled_poa;
                sum_x2 += actual_w * actual_w;
                sum_y2 += modeled_poa * modeled_poa;
                sum_xy += actual_w * modeled_poa;
            }
            
            let num = n * sum_xy - sum_x * sum_y;
            let term1 = (n * sum_x2 - sum_x * sum_x).max(0.0);
            let term2 = (n * sum_y2 - sum_y * sum_y).max(0.0);
            let den = (term1 * term2).sqrt();
            let r = if den > 1e-9 { num / den } else { -1.0 };
            
            if r > best_r {
                best_r = r;
                best_tilt = t;
                best_azimuth = a;
            }
        }
    }
    
    let coarse_tilt = best_tilt;
    let coarse_azimuth = best_azimuth;
    let mut fine_best_r = best_r;
    
    for t_diff in -10..=10 {
        let t = coarse_tilt + (t_diff as f64 * 0.5);
        if t < 0.0 || t > 90.0 { continue; }
        
        for a_diff in -10..=10 {
            let a = (coarse_azimuth + (a_diff as f64 * 0.5) + 360.0) % 360.0;
            
            let mut sum_x = 0.0;
            let mut sum_y = 0.0;
            let mut sum_x2 = 0.0;
            let mut sum_y2 = 0.0;
            let mut sum_xy = 0.0;
            let n = precomputed_solar.len() as f64;
            
            for &(actual_w, el, az) in &precomputed_solar {
                let dni = 900.0 * el.sin().max(0.0);
                let dhi = 120.0 * el.sin().max(0.0);
                let modeled_poa = crate::power_manager::calculate_poa_irradiance(dni, dhi, el, az, t, a);
                sum_x += actual_w;
                sum_y += modeled_poa;
                sum_x2 += actual_w * actual_w;
                sum_y2 += modeled_poa * modeled_poa;
                sum_xy += actual_w * modeled_poa;
            }
            
            let num = n * sum_xy - sum_x * sum_y;
            let term1 = (n * sum_x2 - sum_x * sum_x).max(0.0);
            let term2 = (n * sum_y2 - sum_y * sum_y).max(0.0);
            let den = (term1 * term2).sqrt();
            let r = if den > 1e-9 { num / den } else { -1.0 };
            
            if r > fine_best_r {
                fine_best_r = r;
                best_tilt = t;
                best_azimuth = a;
            }
        }
    }
    
    Ok(Json(serde_json::json!({
        "tilt": (best_tilt * 10.0).round() / 10.0,
        "azimuth": (best_azimuth * 10.0).round() / 10.0,
        "correlation": fine_best_r
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_infer_orientation_no_data() {
        let temp_db = "temp_test_infer.db";
        let _ = std::fs::remove_file(temp_db);
        crate::database::save_config_to_db(temp_db, &crate::config::Config::default_empty()).unwrap();
        crate::database::init_history_db(temp_db).unwrap();

        let req = InferRequest {
            inverter: "solax1".to_string(),
            string: "PV1".to_string(),
            latitude: -33.8688,
            longitude: 151.2093,
        };

        let result = handle_infer_orientation(temp_db.to_string(), Json(req)).await;
        assert!(result.is_err());
        
        let _ = std::fs::remove_file(temp_db);
    }

    #[tokio::test]
    async fn test_infer_orientation_with_mock_telemetry() {
        let temp_db = "temp_test_infer_mock.db";
        let _ = std::fs::remove_file(temp_db);
        crate::database::save_config_to_db(temp_db, &crate::config::Config::default_empty()).unwrap();
        crate::database::init_history_db(temp_db).unwrap();

        let start_time = (chrono::Utc::now().timestamp() / 86400 - 10) * 86400;
        let mut buffer = Vec::new();
        
        for day in 0..10 {
            let day_start = start_time + (day * 24 * 3600);
            for hour in 7..17 {
                let ts = day_start + (hour * 3600);
                let utc_time = chrono::DateTime::from_timestamp(ts, 0).unwrap();
                let (el, az) = crate::power_manager::calculate_solar_position(0.0, 0.0, utc_time);
                let dni = 900.0 * el.sin().max(0.0);
                let dhi = 120.0 * el.sin().max(0.0);
                let poa = crate::power_manager::calculate_poa_irradiance(dni, dhi, el, az, 20.0, 180.0);
                let pv_power = poa * 3.0;
                
                buffer.push(crate::power_manager::HistoryRecord {
                    timestamp: ts,
                    topic: "solax1/PV1 Power".to_string(),
                    value: pv_power,
                });
            }
        }
        crate::database::flush_history_to_db(temp_db, &mut buffer, None);

        let req = InferRequest {
            inverter: "solax1".to_string(),
            string: "PV1".to_string(),
            latitude: 0.0,
            longitude: 0.0,
        };

        let result_json = handle_infer_orientation(temp_db.to_string(), Json(req)).await.unwrap();
        let val = result_json.0;
        
        let tilt = val.get("tilt").unwrap().as_f64().unwrap();
        let azimuth = val.get("azimuth").unwrap().as_f64().unwrap();
        let correlation = val.get("correlation").unwrap().as_f64().unwrap();

        println!("INFERRED VALUES -> Tilt: {}° (expected: 20°), Azimuth: {}° (expected: 180°), Correlation: {}", tilt, azimuth, correlation);

        let _ = std::fs::remove_file(temp_db);

        assert!((tilt - 20.0).abs() < 1.5);
        assert!((azimuth - 180.0).abs() < 1.5);
        assert!(correlation > 0.99);
    }
}
