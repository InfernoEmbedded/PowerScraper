use crate::config::Config;
use axum::{
    Json, Router,
    http::header,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}};
use tokio::sync::mpsc::Sender;
use tower_http::cors::CorsLayer;

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct InverterStatus {
    pub battery_capacity: u8,
    pub battery_power: i32,
    pub pv_power: u32,
    pub run_mode: u32,
    pub last_updated: Option<u64>,
    pub calculated_battery_capacity: Option<f64>,
    pub requested_power: Option<i32>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default)]
pub struct PriceThresholds {
    pub import_30: f64,
    pub import_70: f64,
    pub export_30: f64,
    pub export_70: f64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct SystemStatus {
    pub active_mode: String,
    pub grid_target: f64,
    pub inverters: HashMap<String, InverterStatus>,
    pub meter_power: f64,
    pub meter_last_updated: Option<u64>,
    pub mqtt_connected: bool,
    pub mqtt_enabled: bool,
    pub import_price: Option<f64>,
    pub export_price: Option<f64>,
    pub price_thresholds: Option<PriceThresholds>,
    pub usage: Option<f64>,
    pub power_budget: Option<f64>,
    pub power_budget_with_charging: Option<f64>,
    pub version: String,
}

impl Default for SystemStatus {
    fn default() -> Self {
        SystemStatus {
            active_mode: String::new(),
            grid_target: 0.0,
            inverters: HashMap::new(),
            meter_power: 0.0,
            meter_last_updated: None,
            mqtt_connected: false,
            mqtt_enabled: false,
            import_price: None,
            export_price: None,
            price_thresholds: None,
            usage: None,
            power_budget: None,
            power_budget_with_charging: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

pub static SYSTEM_STATUS: OnceLock<Mutex<SystemStatus>> = OnceLock::new();

pub fn get_system_status() -> &'static Mutex<SystemStatus> {
    SYSTEM_STATUS.get_or_init(|| Mutex::new(SystemStatus::default()))
}

pub async fn get_health_status(db_path: &str) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let config = Config::load_from_db(db_path).unwrap_or_else(|_| Config::default_empty());
    let mut target_inverters = config.get_configured_battery_inverters();

    let status = get_system_status().lock().unwrap().clone();
    if target_inverters.is_empty() {
        target_inverters = status.inverters.keys().cloned().collect();
    }
    target_inverters.sort();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut inverters_info = Vec::new();
    let mut all_healthy = true;
    let mut healthy_count = 0;

    for inv_name in &target_inverters {
        let inv_status = status.inverters.get(inv_name);
        let last_updated = inv_status.and_then(|s| s.last_updated);
        let seconds_ago = last_updated.map(|ts| now.saturating_sub(ts));

        let is_healthy = match seconds_ago {
            Some(sec) => sec <= 60,
            None => false,
        };

        if is_healthy {
            healthy_count += 1;
        } else {
            all_healthy = false;
        }

        inverters_info.push(serde_json::json!({
            "name": inv_name,
            "healthy": is_healthy,
            "last_updated": last_updated,
            "last_updated_seconds_ago": seconds_ago,
        }));
    }

    let status_str = if all_healthy { "healthy" } else { "unhealthy" };
    let http_status = if all_healthy {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };

    (
        http_status,
        Json(serde_json::json!({
            "status": status_str,
            "timestamp": now,
            "inverters_count": target_inverters.len(),
            "healthy_count": healthy_count,
            "inverters": inverters_info,
        })),
    )
}

#[derive(serde::Serialize, Clone)]
#[serde(tag = "type")]
pub enum SimProgressUpdate {
    Progress { percent: f64, eta_seconds: f64 },
    Result { response: crate::power_manager::SimulationResponse },
    Error { message: String },
}

#[derive(serde::Deserialize)]
struct SimQuery {
    range: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct BackupTelemetryRecord {
    pub timestamp: i64,
    pub topic: String,
    pub value: f64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct BackupDump {
    pub version: String,
    pub exported_at: String,
    pub config: Config,
    pub telemetry_history: Vec<BackupTelemetryRecord>,
}

fn export_backup_dump(db_path: &str) -> Result<BackupDump, (axum::http::StatusCode, String)> {
    let cfg = Config::load_from_db(db_path)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to load config: {}", e)))?;

    let _ = crate::database::init_history_db(db_path);

    let history_rows = crate::database::get_all_telemetry_since(db_path, 0)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query telemetry history: {}", e)))?;

    let telemetry_history = history_rows
        .into_iter()
        .map(|(ts, topic, val)| BackupTelemetryRecord {
            timestamp: ts,
            topic,
            value: val,
        })
        .collect();

    Ok(BackupDump {
        version: env!("CARGO_PKG_VERSION").to_string(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        config: cfg,
        telemetry_history,
    })
}

fn export_telemetry_csv(db_path: &str) -> Result<([(header::HeaderName, String); 2], Vec<u8>), (axum::http::StatusCode, String)> {
    let _ = crate::database::init_history_db(db_path);

    let cfg = Config::load_from_db(db_path).unwrap_or_else(|_| Config::default_empty());
    let tz_offset = crate::power_manager::get_timezone_offset(cfg.battery_control.as_ref().and_then(|bc| bc.timezone.as_deref()));

    let history_rows = crate::database::get_all_telemetry_since(db_path, 0)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query telemetry history: {}", e)))?;

    let mut topics_set = std::collections::BTreeSet::new();
    let mut time_map: std::collections::BTreeMap<i64, std::collections::HashMap<String, f64>> = std::collections::BTreeMap::new();

    for (ts, topic, val) in history_rows {
        topics_set.insert(topic.clone());
        time_map.entry(ts).or_default().insert(topic, val);
    }

    let topics: Vec<String> = topics_set.into_iter().collect();

    let mut wtr = csv::WriterBuilder::new().from_writer(Vec::new());

    let mut header_row = vec!["timestamp".to_string(), "datetime".to_string()];
    header_row.extend(topics.clone());

    if let Err(e) = wtr.write_record(&header_row) {
        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("CSV write header error: {}", e)));
    }

    for (ts, vals) in time_map {
        let dt_str = match chrono::DateTime::from_timestamp(ts, 0) {
            Some(dt) => dt.with_timezone(&tz_offset).to_rfc3339(),
            None => "".to_string(),
        };

        let mut row = Vec::with_capacity(2 + topics.len());
        row.push(ts.to_string());
        row.push(dt_str);

        for topic in &topics {
            if let Some(val) = vals.get(topic) {
                row.push(val.to_string());
            } else {
                row.push("".to_string());
            }
        }

        if let Err(e) = wtr.write_record(&row) {
            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("CSV write record error: {}", e)));
        }
    }

    let csv_bytes = wtr.into_inner()
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("CSV flush error: {}", e)))?;

    let now_str = chrono::Utc::now().with_timezone(&tz_offset).format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("powerscraper_telemetry_{}.csv", now_str);

    let headers = [
        (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        ),
    ];

    Ok((headers, csv_bytes))
}

pub fn build_web_app(reload_tx: Sender<()>, db_path: String) -> Router {
    let db_path_clone = db_path.clone();
    let db_path_backup_export = db_path.clone();
    let db_path_backup_dl = db_path.clone();
    let db_path_telemetry_csv = db_path.clone();
    let db_path_telemetry_csv2 = db_path.clone();
    let db_path_backup_import = db_path.clone();
    let db_path_history_api = db_path.clone();
    let db_path_sim = db_path.clone();
    let db_path_train = db_path.clone();
    let db_path_apply = db_path.clone();
    let db_path_infer = db_path.clone();
    let db_path_debug = db_path.clone();
    let db_path_debug2 = db_path.clone();
    let reload_tx_apply = reload_tx.clone();
    let state = Arc::new(reload_tx);

    Router::new()
        .route("/", get(serve_dashboard))
        .route("/style.css", get(serve_style))
        .route("/app.js", get(serve_js))
        .route(
            "/api/mqtt/test",
            post(handle_mqtt_test),
        )
        .route(
            "/api/simulation/run",
            get(move |axum::extract::Query(query): axum::extract::Query<SimQuery>| {
                let path = db_path_sim.clone();
                async move {
                    let range_str = query.range.as_deref().unwrap_or("1m").to_string();
                    let (tx, rx) = tokio::sync::mpsc::channel(100);

                    tokio::task::spawn_blocking(move || {
                        let progress_cb = |percent: f64, eta_seconds: f64| {
                            let _ = tx.blocking_send(SimProgressUpdate::Progress { percent, eta_seconds });
                        };
                        match crate::power_manager::run_historical_simulation_impl(&path, &range_str, Some(&progress_cb)) {
                            Ok(res) => {
                                let _ = tx.blocking_send(SimProgressUpdate::Result { response: res });
                            }
                            Err(e) => {
                                let _ = tx.blocking_send(SimProgressUpdate::Error { message: e });
                            }
                        }
                    });

                    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                        match rx.recv().await {
                            Some(item) => {
                                match axum::response::sse::Event::default().json_data(item) {
                                    Ok(ev) => Some((Ok::<axum::response::sse::Event, std::convert::Infallible>(ev), rx)),
                                    Err(_) => None,
                                }
                            }
                            None => None,
                        }
                    });

                    axum::response::Sse::new(stream)
                        .keep_alive(axum::response::sse::KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/config/import",
            post(|body: String| async move {
                match toml::from_str::<Config>(&body) {
                    Ok(cfg) => Ok(Json(cfg)),
                    Err(e) => Err((axum::http::StatusCode::BAD_REQUEST, e.to_string())),
                }
            }),
        )
        .route(
            "/api/config",
            get(move || {
                let path = db_path_clone.clone();
                async move {
                    match Config::load_from_db(&path) {
                        Ok(cfg) => Ok(Json(cfg)),
                        Err(e) => {
                            Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
                        }
                    }
                }
            }),
        )
        .route(
            "/api/config",
            post({
                let reload_channel = state.clone();
                move |Json(new_cfg): Json<Config>| {
                    let path = db_path.clone();
                    let reload_channel = reload_channel.clone();
                    async move {
                        if let Err(e) = new_cfg.save_to_db(&path) {
                            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                        }
                        // Signal live reload to daemon tasks
                        let _ = reload_channel.send(()).await;
                        Ok(Json(serde_json::json!({ "status": "success" })))
                    }
                }
            }),
        )
        .route(
            "/api/backup/export",
            get({
                let db_path = db_path_backup_export.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        export_backup_dump(&path).map(Json)
                    }
                }
            }),
        )
        .route(
            "/api/backup/download",
            get({
                let db_path = db_path_backup_dl.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        match export_backup_dump(&path) {
                            Ok(dump) => {
                                let now_str = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
                                let filename = format!("powerscraper_backup_{}.json.xz", now_str);
                                let json_bytes = match serde_json::to_vec_pretty(&dump) {
                                    Ok(b) => b,
                                    Err(e) => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                                };
                                let mut compressed_bytes = Vec::new();
                                if let Err(e) = lzma_rs::xz_compress(&mut std::io::Cursor::new(json_bytes), &mut compressed_bytes) {
                                    return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("XZ compression failed: {}", e)));
                                }
                                let headers = [
                                    (header::CONTENT_TYPE, "application/x-xz".to_string()),
                                    (
                                        header::CONTENT_DISPOSITION,
                                        format!("attachment; filename=\"{}\"", filename),
                                    ),
                                ];
                                Ok((headers, compressed_bytes))
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
            }),
        )
        .route(
            "/api/telemetry/export.csv",
            get({
                let db_path = db_path_telemetry_csv.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        export_telemetry_csv(&path)
                    }
                }
            }),
        )
        .route(
            "/api/backup/telemetry.csv",
            get({
                let db_path = db_path_telemetry_csv2.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        export_telemetry_csv(&path)
                    }
                }
            }),
        )
        .route(
            "/api/backup/import",
            post({
                let db_path = db_path_backup_import.clone();
                let reload_channel = state.clone();
                move |body_bytes: axum::body::Bytes| {
                    let path = db_path.clone();
                    let reload_channel = reload_channel.clone();
                    async move {
                        let mut decompressed_bytes = Vec::new();
                        let data_slice: &[u8] = if lzma_rs::xz_decompress(&mut std::io::Cursor::new(&body_bytes), &mut decompressed_bytes).is_ok() {
                            &decompressed_bytes
                        } else {
                            &body_bytes
                        };

                        let (cfg, records) = match serde_json::from_slice::<BackupDump>(data_slice) {
                            Ok(dump) => {
                                let recs: Vec<(i64, String, f64)> = dump
                                    .telemetry_history
                                    .into_iter()
                                    .map(|r| (r.timestamp, r.topic, r.value))
                                    .collect();
                                (dump.config, recs)
                            }
                            Err(_) => {
                                let cfg = serde_json::from_slice::<Config>(data_slice)
                                    .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("Failed to parse backup JSON: {}", e)))?;
                                (cfg, Vec::new())
                            }
                        };

                        if let Err(e) = cfg.save_to_db(&path) {
                            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save config: {}", e)));
                        }

                        let count = records.len();
                        if count > 0 {
                            if let Err(e) = crate::database::insert_telemetry_history_batch(&path, &records) {
                                return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to import telemetry history: {}", e)));
                            }
                        }

                        let _ = reload_channel.send(()).await;

                        Ok(Json(serde_json::json!({
                            "status": "success",
                            "imported_telemetry_records": count
                        })))
                    }
                }
            }),
        )
        .route(
            "/api/history",
            get({
                let db_path = db_path_history_api.clone();
                move |axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>| {
                    let path = db_path.clone();
                    async move {
                        let now = chrono::Utc::now().timestamp();
                        let start_ts = params.get("start")
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(now - 86400);
                        let end_ts = params.get("end")
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(now);

                        let max_pixels = params.get("max_pixels")
                            .or_else(|| params.get("pixels"))
                            .or_else(|| params.get("width"))
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(1200);

                        let topics_opt: Option<Vec<String>> = params.get("topics")
                            .map(|s| {
                                let mut result = String::with_capacity(s.len());
                                let bytes = s.as_bytes();
                                let mut i = 0;
                                while i < bytes.len() {
                                    if bytes[i] == b'%' && i + 2 < bytes.len() {
                                        if let Ok(val) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                                            result.push(val as char);
                                            i += 3;
                                            continue;
                                        }
                                    }
                                    if bytes[i] == b'+' {
                                        result.push(' ');
                                    } else {
                                        result.push(bytes[i] as char);
                                    }
                                    i += 1;
                                }
                                result.split(',')
                                    .map(|t| t.trim().to_string())
                                    .filter(|t| !t.is_empty())
                                    .collect()
                            });

                        let req_start = std::time::Instant::now();
                        let res = tokio::task::spawn_blocking(move || {
                            crate::database::get_decimated_telemetry_in_range_profiled(&path, start_ts, end_ts, max_pixels, topics_opt.as_deref())
                        }).await;

                        match res {
                            Ok(Ok((records, raw_count, db_dur, decimate_dur))) => {
                                let t_fmt = std::time::Instant::now();
                                #[derive(serde::Serialize)]
                                struct TelemetryRecordRef<'a> {
                                    timestamp: i64,
                                    topic: &'a str,
                                    value: f64,
                                }
                                let refs: Vec<TelemetryRecordRef> = records
                                    .iter()
                                    .map(|(ts, topic, val)| TelemetryRecordRef {
                                        timestamp: *ts,
                                        topic: topic.as_str(),
                                        value: *val,
                                    })
                                    .collect();
                                let json_body = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".to_string());
                                let fmt_dur = t_fmt.elapsed().as_secs_f64() * 1000.0;
                                let total_dur = req_start.elapsed().as_secs_f64() * 1000.0;

                                println!(
                                    "[Profile /api/history] DB Query: {:.2}ms ({} rows), Decimation: {:.2}ms ({} decimated rows), Format: {:.2}ms, Total Server: {:.2}ms, Payload: {} bytes",
                                    db_dur, raw_count, decimate_dur, refs.len(), fmt_dur, total_dur, json_body.len()
                                );

                                let server_timing = format!(
                                    "db;dur={:.2};desc=\"DB Query\", decimate;dur={:.2};desc=\"Decimation\", format;dur={:.2};desc=\"JSON Format\", total;dur={:.2};desc=\"Total Server\"",
                                    db_dur, decimate_dur, fmt_dur, total_dur
                                );

                                Ok((
                                    [
                                        (axum::http::header::HeaderName::from_static("server-timing"), server_timing),
                                        (axum::http::header::CONTENT_TYPE, "application/json".to_string()),
                                    ],
                                    json_body,
                                ))
                            }
                            Ok(Err(e)) => Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Database query error: {}", e))),
                            Err(e) => Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Blocking task error: {}", e))),
                        }
                    }
                }
            }),
        )
        .route(
            "/api/status",
            get(|| async {
                let lock = get_system_status().lock().unwrap();
                Json(lock.clone())
            }),
        )
        .route(
            "/api/health",
            get({
                let db_path = db_path_debug.clone();
                move || {
                    let path = db_path.clone();
                    async move { get_health_status(&path).await }
                }
            }),
        )
        .route(
            "/health",
            get({
                let db_path = db_path_debug.clone();
                move || {
                    let path = db_path.clone();
                    async move { get_health_status(&path).await }
                }
            }),
        )
        .route(
            "/api/debug",
            get({
                let db_path = db_path_debug.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        let config = Config::load_from_db(&path).unwrap_or_else(|_| Config::default_empty());
                        let status = get_system_status().lock().unwrap().clone();

                        let mut data_sources = Vec::new();
                        if let Some(ref wifi) = config.solax_wifi {
                            for host in &wifi.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": host,
                                    "type": "solax_wifi",
                                    "status": status.inverters.get(host),
                                }));
                            }
                        }
                        if let Some(ref mb) = config.solax_modbus {
                            for inv in &mb.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g4) = config.solax_g4_modbus {
                            for inv in &g4.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g4_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g3) = config.solax_g3_modbus {
                            for inv in &g3.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g3_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref sdm) = config.sdm630_modbus_v2 {
                            for port in &sdm.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "sdm630_modbus_v2",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref dtsu) = config.dtsu666 {
                            for port in &dtsu.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "dtsu666",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref meter) = config.mqtt_power_meter {
                            for name in &meter.meters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_power_meter",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref inv) = config.mqtt_inverter {
                            for name in &inv.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_inverter",
                                    "status": status.inverters.get(name),
                                }));
                            }
                        }

                        let debug_payload = serde_json::json!({
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "timestamp_epoch": chrono::Utc::now().timestamp(),
                            "config": config,
                            "system_status": status,
                            "data_sources": data_sources,
                            "power_manager_internal_state": {
                                "active_mode": status.active_mode,
                                "grid_target": status.grid_target,
                                "inverters": status.inverters,
                                "meter_power": status.meter_power,
                                "meter_last_updated": status.meter_last_updated,
                                "import_price": status.import_price,
                                "export_price": status.export_price,
                                "price_thresholds": status.price_thresholds,
                                "usage": status.usage,
                                "power_budget": status.power_budget,
                                "power_budget_with_charging": status.power_budget_with_charging,
                            }
                        });

                        Json(debug_payload)
                    }
                }
            }),
        )
        .route(
            "/debug",
            get({
                let db_path = db_path_debug2.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        let config = Config::load_from_db(&path).unwrap_or_else(|_| Config::default_empty());
                        let status = get_system_status().lock().unwrap().clone();

                        let mut data_sources = Vec::new();
                        if let Some(ref wifi) = config.solax_wifi {
                            for host in &wifi.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": host,
                                    "type": "solax_wifi",
                                    "status": status.inverters.get(host),
                                }));
                            }
                        }
                        if let Some(ref mb) = config.solax_modbus {
                            for inv in &mb.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g4) = config.solax_g4_modbus {
                            for inv in &g4.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g4_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g3) = config.solax_g3_modbus {
                            for inv in &g3.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g3_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref sdm) = config.sdm630_modbus_v2 {
                            for port in &sdm.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "sdm630_modbus_v2",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref dtsu) = config.dtsu666 {
                            for port in &dtsu.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "dtsu666",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref meter) = config.mqtt_power_meter {
                            for name in &meter.meters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_power_meter",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref inv) = config.mqtt_inverter {
                            for name in &inv.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_inverter",
                                    "status": status.inverters.get(name),
                                }));
                            }
                        }

                        let debug_payload = serde_json::json!({
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "timestamp_epoch": chrono::Utc::now().timestamp(),
                            "config": config,
                            "system_status": status,
                            "data_sources": data_sources,
                            "power_manager_internal_state": {
                                "active_mode": status.active_mode,
                                "grid_target": status.grid_target,
                                "inverters": status.inverters,
                                "meter_power": status.meter_power,
                                "meter_last_updated": status.meter_last_updated,
                                "import_price": status.import_price,
                                "export_price": status.export_price,
                                "price_thresholds": status.price_thresholds,
                                "usage": status.usage,
                                "power_budget": status.power_budget,
                                "power_budget_with_charging": status.power_budget_with_charging,
                            }
                        });

                        Json(debug_payload)
                    }
                }
            }),
        )
        .route(
            "/api/location/infer-orientation",
            post({
                let db_path_clone = db_path_infer.clone();
                move |body: Json<crate::orientation_inference::InferRequest>| async move {
                    crate::orientation_inference::handle_infer_orientation(db_path_clone, body).await
                }
            }),
        )
        .route(
            "/api/train/start",
            post({
                let db_path = db_path_train.clone();
                move |Json(req): Json<StartTrainingRequest>| {
                    let db_path = db_path.clone();
                    async move {
                        handle_start_training(db_path, req).await
                    }
                }
            })
        )
        .route(
            "/api/train/cancel",
            post(handle_cancel_training)
        )
        .route(
            "/api/train/apply",
            post({
                let db_path = db_path_apply.clone();
                let reload_tx = reload_tx_apply.clone();
                move |Json(params): Json<ApplyParamsRequest>| {
                    let db_path = db_path.clone();
                    let reload_tx = reload_tx.clone();
                    async move {
                        handle_apply_training(reload_tx, db_path, params).await
                    }
                }
            })
        )
        .route(
            "/api/train/status",
            get(|| async {
                let lock = get_tuning_progress().lock().unwrap();
                Json(lock.clone())
            })
        )
        .route(
            "/api/train/progress",
            get(handle_training_progress)
        )
        .layer(CorsLayer::permissive())
}

pub async fn run_web_server_with_listener(reload_tx: Sender<()>, db_path: String, listener: tokio::net::TcpListener) {
    let app = build_web_app(reload_tx, db_path);
    axum::serve(listener, app).await.unwrap();
}

#[derive(serde::Serialize, Clone, Debug, Default)]
pub struct TuningProgress {
    pub is_running: bool,
    pub last_generation: u32,
    pub total_generations: u32,
    pub percent: f64,
    pub best_cost: f64,
    pub bill: f64,
    pub cycles: f64,
    pub logs: Vec<String>,
    pub best_params: Option<crate::config::EvolvedHeuristicConfig>,
    pub best_params_monthly: Option<HashMap<String, crate::config::EvolvedHeuristicConfig>>,
    pub error: Option<String>,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct TuningLogEvent {
    pub percent: f64,
    #[serde(rename = "gen")]
    pub gen_num: u32,
    pub total_gens: u32,
    pub best_cost: f64,
    pub bill: f64,
    pub cycles: f64,
    pub log_line: String,
    pub done: bool,
    pub error: Option<String>,
    pub best_params: Option<crate::config::EvolvedHeuristicConfig>,
    pub best_params_monthly: Option<HashMap<String, crate::config::EvolvedHeuristicConfig>>,
}

pub static TUNING_PROGRESS: OnceLock<Mutex<TuningProgress>> = OnceLock::new();
pub static ACTIVE_CHILD: OnceLock<Mutex<Option<std::process::Child>>> = OnceLock::new();
pub static TUNING_CHANNEL: OnceLock<tokio::sync::broadcast::Sender<TuningLogEvent>> = OnceLock::new();

pub fn get_tuning_progress() -> &'static Mutex<TuningProgress> {
    TUNING_PROGRESS.get_or_init(|| Mutex::new(TuningProgress::default()))
}

pub fn get_active_child() -> &'static Mutex<Option<std::process::Child>> {
    ACTIVE_CHILD.get_or_init(|| Mutex::new(None))
}

pub fn get_tuning_channel() -> &'static tokio::sync::broadcast::Sender<TuningLogEvent> {
    TUNING_CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        tx
    })
}

#[derive(serde::Deserialize)]
pub struct StartTrainingRequest {
    pub seed: bool,
    pub generations: u32,
    pub population_size: u32,
    pub cycle_penalty: f64,
    pub cores: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
pub enum ApplyParamsRequest {
    Single(crate::config::EvolvedHeuristicConfig),
    Monthly(HashMap<String, crate::config::EvolvedHeuristicConfig>),
}

pub static CANCEL_TUNING: AtomicBool = AtomicBool::new(false);

pub async fn handle_start_training(
    db_path: String,
    req: StartTrainingRequest,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mut progress = get_tuning_progress().lock().unwrap();
    if progress.is_running {
        return Err((axum::http::StatusCode::CONFLICT, "Training is already running".to_string()));
    }

    // Parse seed if requested
    let mut seed_config = None;
    let mut seed_config_monthly = None;
    if req.seed {
        if let Ok(cfg) = Config::load_from_db(&db_path) {
            if let Some(bc) = cfg.battery_control {
                seed_config = bc.evolved_heuristic.clone();
                seed_config_monthly = bc.evolved_heuristic_monthly.clone();
            }
        }
    }

    // Reset cancellation flag
    CANCEL_TUNING.store(false, Ordering::Relaxed);

    let db_path_clone = db_path.clone();
    let req_generations = req.generations;
    let req_population_size = req.population_size;
    let req_cycle_penalty = req.cycle_penalty;

    // Reset progress state
    *progress = TuningProgress {
        is_running: true,
        total_generations: req_generations,
        ..Default::default()
    };

    let tx = get_tuning_channel().clone();

    // Spawn tuning thread
    tokio::task::spawn_blocking(move || {
        let (records, sim_config) = match crate::power_manager::load_sim_records(&db_path_clone, "all") {
            Ok(res) => res,
            Err(e) => {
                let mut progress = get_tuning_progress().lock().unwrap();
                progress.is_running = false;
                progress.error = Some(e.clone());
                let _ = tx.send(TuningLogEvent {
                    percent: 0.0,
                    gen_num: 0,
                    total_gens: req_generations,
                    best_cost: 0.0,
                    bill: 0.0,
                    cycles: 0.0,
                    log_line: format!("Error loading data: {}", e),
                    done: true,
                    error: Some(e),
                    best_params: None,
                    best_params_monthly: None,
                });
                return;
            }
        };

        let progress_tx = tx.clone();
        let progress_cb = move |event: crate::simulation::tuning::TuningProgressEvent| -> bool {
            if CANCEL_TUNING.load(Ordering::Relaxed) {
                return false;
            }

            let log_event = TuningLogEvent {
                percent: event.percent,
                gen_num: event.gen_num,
                total_gens: event.total_gens,
                best_cost: event.best_cost,
                bill: event.bill,
                cycles: event.cycles,
                log_line: event.log_line.clone(),
                done: event.done,
                error: None,
                best_params: event.best_params.clone(),
                best_params_monthly: event.best_params_monthly.clone(),
            };

            if let Ok(mut prog) = get_tuning_progress().lock() {
                prog.last_generation = event.gen_num;
                prog.percent = event.percent;
                prog.best_cost = event.best_cost;
                prog.bill = event.bill;
                prog.cycles = event.cycles;
                prog.logs.push(event.log_line);
                if event.done {
                    prog.is_running = false;
                    prog.best_params = event.best_params.clone();
                    prog.best_params_monthly = event.best_params_monthly.clone();
                }
            }

            let _ = progress_tx.send(log_event);
            true
        };

        match crate::simulation::tuning::run_tuning(
            &records,
            &sim_config,
            req_generations,
            req_population_size,
            req_cycle_penalty,
            seed_config,
            seed_config_monthly,
            Some(&progress_cb),
        ) {
            Ok(best_params_monthly) => {
                if CANCEL_TUNING.load(Ordering::Relaxed) {
                    return;
                }
                
                let mut progress = get_tuning_progress().lock().unwrap();
                progress.is_running = false;
                progress.best_params_monthly = Some(best_params_monthly.clone());

                let _ = tx.send(TuningLogEvent {
                    percent: 100.0,
                    gen_num: progress.last_generation,
                    total_gens: progress.total_generations,
                    best_cost: progress.best_cost,
                    bill: progress.bill,
                    cycles: progress.cycles,
                    log_line: "Tuning successfully completed in Rust for all months.".to_string(),
                    done: true,
                    error: None,
                    best_params: None,
                    best_params_monthly: Some(best_params_monthly),
                });
            }
            Err(e) => {
                let mut progress = get_tuning_progress().lock().unwrap();
                progress.is_running = false;
                if e == "Tuning cancelled by user" {
                    progress.error = Some("Cancelled by user".to_string());
                } else {
                    progress.error = Some(e.clone());
                }

                let _ = tx.send(TuningLogEvent {
                    percent: progress.percent,
                    gen_num: progress.last_generation,
                    total_gens: progress.total_generations,
                    best_cost: progress.best_cost,
                    bill: progress.bill,
                    cycles: progress.cycles,
                    log_line: format!("Error during tuning: {}", e),
                    done: true,
                    error: Some(e),
                    best_params: None,
                    best_params_monthly: None,
                });
            }
        }
    });

    Ok(Json(serde_json::json!({ "status": "started" })))
}

pub async fn handle_cancel_training() -> impl IntoResponse {
    CANCEL_TUNING.store(true, Ordering::Relaxed);

    let mut prog = get_tuning_progress().lock().unwrap();
    prog.is_running = false;
    prog.error = Some("Training cancelled by user".to_string());

    let _ = get_tuning_channel().send(TuningLogEvent {
        percent: prog.percent,
        gen_num: prog.last_generation,
        total_gens: prog.total_generations,
        best_cost: prog.best_cost,
        bill: prog.bill,
        cycles: prog.cycles,
        log_line: "Training cancelled by user.".to_string(),
        done: true,
        error: Some("Cancelled by user".to_string()),
        best_params: None,
        best_params_monthly: None,
    });

    Json(serde_json::json!({ "status": "cancelled" }))
}

pub async fn handle_apply_training(
    reload_tx: Sender<()>,
    db_path: String,
    params: ApplyParamsRequest,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mut cfg = match Config::load_from_db(&db_path) {
        Ok(c) => c,
        Err(e) => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to load config: {}", e))),
    };

    if let Some(ref mut bc) = cfg.battery_control {
        match params {
            ApplyParamsRequest::Single(single) => {
                bc.evolved_heuristic = Some(single);
            }
            ApplyParamsRequest::Monthly(monthly) => {
                bc.evolved_heuristic_monthly = Some(monthly);
            }
        }
    } else {
        return Err((axum::http::StatusCode::BAD_REQUEST, "Battery control config not initialized in database settings.".to_string()));
    }

    if let Err(e) = cfg.save_to_db(&db_path) {
        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save config: {}", e)));
    }

    let _ = reload_tx.send(()).await;
    Ok(Json(serde_json::json!({ "status": "applied" })))
}

pub async fn handle_training_progress() -> impl IntoResponse {
    let rx = get_tuning_channel().subscribe();
    
    let logs = {
        let prog = get_tuning_progress().lock().unwrap();
        prog.logs.clone()
    };

    let stream = futures_util::stream::unfold((rx, logs, 0), |(mut rx, logs, mut sent_logs_idx)| async move {
        if sent_logs_idx < logs.len() {
            let log_line = logs[sent_logs_idx].clone();
            let event = TuningLogEvent {
                percent: 0.0,
                gen_num: 0,
                total_gens: 0,
                best_cost: 0.0,
                bill: 0.0,
                cycles: 0.0,
                log_line,
                done: false,
                error: None,
                best_params: None,
                best_params_monthly: None,
            };
            sent_logs_idx += 1;
            return Some((Ok::<axum::response::sse::Event, std::convert::Infallible>(
                axum::response::sse::Event::default().json_data(event).unwrap()
            ), (rx, logs, sent_logs_idx)));
        }

        match rx.recv().await {
            Ok(item) => {
                let ev = axum::response::sse::Event::default().json_data(item).unwrap();
                Some((Ok(ev), (rx, logs, sent_logs_idx)))
            }
            Err(_) => None,
        }
    });

    axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
}

pub async fn run_web_server(reload_tx: Sender<()>, db_path: String) {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Web UI and REST API running at http://localhost:3000");
    run_web_server_with_listener(reload_tx, db_path, listener).await;
}

async fn serve_dashboard() -> Html<&'static str> {
    Html(crate::web_assets::INDEX_HTML)
}

async fn serve_style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css")],
        crate::web_assets::STYLE_CSS,
    )
}

async fn serve_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        crate::web_assets::APP_JS,
    )
}

#[derive(serde::Serialize)]
struct MqttTestResponse {
    connected: bool,
    error: Option<String>,
    wildcard_subscription: bool,
    base_pub_sub: bool,
    ha_discovery: Option<bool>,
}

async fn handle_mqtt_test(
    Json(config): Json<crate::config::MqttBrokerConfig>,
) -> impl IntoResponse {
    let client_id = format!(
        "powerscraper-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    let (client, mut eventloop) = crate::mqtt_helper::create_mqtt_client(&client_id, &config);

    let base_topic = config.base_topic.clone().unwrap_or_else(|| "sensors".to_string());
    let wildcard_topic = format!("{}/#", base_topic);
    let pubsub_topic = format!("{}/test/pubsub", base_topic);
    let wildcard_test_topic = format!("{}/test/wildcard-only", base_topic);
    
    let ha_enabled = config.is_ha_discovery_enabled();
    let ha_prefix = config.ha_discovery_prefix();
    let ha_test_topic = format!("{}/sensor/test/config", ha_prefix);

    let wildcard_token = format!("token-wildcard-{}", client_id);
    let pubsub_token = format!("token-pubsub-{}", client_id);
    let ha_token = format!("token-ha-{}", client_id);

    // Attempt subscriptions
    if let Err(e) = client.subscribe(&wildcard_topic, rumqttc::QoS::AtLeastOnce).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to initiate wildcard subscription: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if let Err(e) = client.subscribe(&pubsub_topic, rumqttc::QoS::AtLeastOnce).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to initiate pubsub subscription: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if ha_enabled {
        if let Err(e) = client.subscribe(&ha_test_topic, rumqttc::QoS::AtLeastOnce).await {
            return Json(MqttTestResponse {
                connected: false,
                error: Some(format!("Failed to initiate HA discovery subscription: {}", e)),
                wildcard_subscription: false,
                base_pub_sub: false,
                ha_discovery: Some(false),
            });
        }
    }

    // Attempt publications
    if let Err(e) = client.publish(&wildcard_test_topic, rumqttc::QoS::AtLeastOnce, false, wildcard_token.clone()).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to publish to wildcard test topic: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if let Err(e) = client.publish(&pubsub_topic, rumqttc::QoS::AtLeastOnce, false, pubsub_token.clone()).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to publish to pubsub topic: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if ha_enabled {
        if let Err(e) = client.publish(&ha_test_topic, rumqttc::QoS::AtLeastOnce, false, ha_token.clone()).await {
            return Json(MqttTestResponse {
                connected: false,
                error: Some(format!("Failed to publish to HA discovery topic: {}", e)),
                wildcard_subscription: false,
                base_pub_sub: false,
                ha_discovery: Some(false),
            });
        }
    }

    let mut connected = false;
    let mut wildcard_subscription = false;
    let mut base_pub_sub = false;
    let mut ha_discovery = if ha_enabled { Some(false) } else { None };
    let mut error = None;

    let start_time = std::time::Instant::now();
    let timeout_duration = std::time::Duration::from_secs(3);

    while start_time.elapsed() < timeout_duration {
        let done = wildcard_subscription && base_pub_sub && (!ha_enabled || ha_discovery == Some(true));
        if done {
            break;
        }

        let remaining = timeout_duration.saturating_sub(start_time.elapsed());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, eventloop.poll()).await {
            Ok(Ok(notification)) => {
                connected = true;
                match notification {
                    rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)) => {
                        let payload = String::from_utf8_lossy(&p.payload);
                        if p.topic == wildcard_test_topic && payload == wildcard_token {
                            wildcard_subscription = true;
                        } else if p.topic == pubsub_topic && payload == pubsub_token {
                            base_pub_sub = true;
                        } else if p.topic == ha_test_topic && payload == ha_token {
                            ha_discovery = Some(true);
                        }
                    }
                    rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(connack)) => {
                        if connack.code != rumqttc::ConnectReturnCode::Success {
                            error = Some(format!("Connection refused: {:?}", connack.code));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Err(e)) => {
                error = Some(format!("Connection/Protocol error: {}", e));
                break;
            }
            Err(_) => {
                // Timeout
                break;
            }
        }
    }

    let _ = client.disconnect().await;

    // If we didn't receive packets but did not get a specific connection/protocol error,
    // we can formulate an informative message.
    if connected && error.is_none() {
        let mut missing = Vec::new();
        if !wildcard_subscription {
            missing.push(format!("wildcard sub ({})", wildcard_topic));
        }
        if !base_pub_sub {
            missing.push(format!("base pub/sub ({})", pubsub_topic));
        }
        if ha_enabled && ha_discovery != Some(true) {
            missing.push(format!("HA discovery pub/sub ({})", ha_test_topic));
        }
        if !missing.is_empty() {
            error = Some(format!("Permission denied or message delivery failed for: {}", missing.join(", ")));
        }
    } else if !connected && error.is_none() {
        error = Some("Connection timed out".to_string());
    }

    Json(MqttTestResponse {
        connected,
        error,
        wildcard_subscription,
        base_pub_sub,
        ha_discovery,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt; // for oneshot/call

    #[tokio::test]
    async fn test_web_server_routes_and_errors() {
        let (reload_tx, _) = tokio::sync::mpsc::channel(10);
        let temp_db = "temp_test_web_server.db";
        let _ = std::fs::remove_file(temp_db);

        crate::config::Config::default_empty().save_to_db(temp_db).unwrap();

        let app = build_web_app(reload_tx, temp_db.to_string());

        // 1. Test GET / (Dashboard)
        let response = app.clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 2. Test GET /nonexistent (404)
        let response = app.clone()
            .oneshot(Request::builder().uri("/nonexistent").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // 3. Test POST /api/config/import with invalid TOML syntax (400)
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/import")
                    .header("content-type", "text/plain")
                    .body(Body::from("invalid_toml_value = =="))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // 4. Test POST /api/config with malformed JSON body (422 or 400)
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config")
                    .header("content-type", "application/json")
                    .body(Body::from("{invalid json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status() == StatusCode::UNPROCESSABLE_ENTITY || response.status() == StatusCode::BAD_REQUEST);

        // 5. Test POST /api/config with unsupported content type (415)
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config")
                    .header("content-type", "text/plain")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // 6. Test GET /api/backup/export
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/backup/export").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 7. Test GET /api/backup/download
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/backup/download").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("content-disposition"));

        // 8. Test POST /api/backup/import (uncompressed JSON and compressed XZ)
        let dump_json = serde_json::json!({
            "version": "1.0.84",
            "exported_at": "2026-08-01T00:00:00Z",
            "config": crate::config::Config::default_empty(),
            "telemetry_history": [
                { "timestamp": 1000, "topic": "test/topic", "value": 42.0 }
            ]
        });
        let raw_json_bytes = dump_json.to_string().into_bytes();

        // 8a. Test uncompressed JSON import
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/backup/import")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(raw_json_bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 8b. Test XZ compressed backup import
        let mut xz_bytes = Vec::new();
        lzma_rs::xz_compress(&mut std::io::Cursor::new(raw_json_bytes), &mut xz_bytes).unwrap();
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/backup/import")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(xz_bytes))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 9. Test GET /api/telemetry/export.csv
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/telemetry/export.csv").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/csv; charset=utf-8");
        assert!(response.headers().contains_key("content-disposition"));

        // 10. Test GET /api/debug and /debug
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/debug").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 11. Test GET /api/health and /health
        {
            get_system_status().lock().unwrap().inverters.clear();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            get_system_status().lock().unwrap().inverters.insert("solax-x1".to_string(), InverterStatus {
                last_updated: Some(now - 5),
                ..Default::default()
            });

            let response = app.clone()
                .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let response = app.clone()
                .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            // Test stale inverter (>60s ago)
            get_system_status().lock().unwrap().inverters.insert("solax-x1".to_string(), InverterStatus {
                last_updated: Some(now - 120),
                ..Default::default()
            });

            let response = app.clone()
                .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }

        let _ = std::fs::remove_file(temp_db);
    }
}


