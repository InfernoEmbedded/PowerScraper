use crate::config::{Config, EvolvedHeuristicConfig};
use axum::{
    Json, Router,
    http::header,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
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
    pub import_price: Option<f64>,
    pub export_price: Option<f64>,
    pub price_thresholds: Option<PriceThresholds>,
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
            import_price: None,
            export_price: None,
            price_thresholds: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

pub static SYSTEM_STATUS: OnceLock<Mutex<SystemStatus>> = OnceLock::new();

pub fn get_system_status() -> &'static Mutex<SystemStatus> {
    SYSTEM_STATUS.get_or_init(|| Mutex::new(SystemStatus::default()))
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

pub async fn run_web_server_with_listener(reload_tx: Sender<()>, db_path: String, listener: tokio::net::TcpListener) {
    let db_path_clone = db_path.clone();
    let db_path_sim = db_path.clone();
    let db_path_train = db_path.clone();
    let db_path_apply = db_path.clone();
    let reload_tx_apply = reload_tx.clone();
    let state = Arc::new(reload_tx);

    let app = Router::new()
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
            post(move |Json(new_cfg): Json<Config>| {
                let path = db_path.clone();
                let reload_channel = state.clone();
                async move {
                    if let Err(e) = new_cfg.save_to_db(&path) {
                        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                    }
                    // Signal live reload to daemon tasks
                    let _ = reload_channel.send(()).await;
                    Ok(Json(serde_json::json!({ "status": "success" })))
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
                move |Json(params): Json<EvolvedHeuristicConfig>| {
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
        .layer(CorsLayer::permissive());

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

pub async fn handle_start_training(
    db_path: String,
    req: StartTrainingRequest,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mut progress = get_tuning_progress().lock().unwrap();
    if progress.is_running {
        return Err((axum::http::StatusCode::CONFLICT, "Training is already running".to_string()));
    }

    // Check if script exists
    let script_paths = [
        "scripts/evolutionary_optimizer.py",
        "/usr/share/powerscraper/scripts/evolutionary_optimizer.py",
    ];
    let mut script_path = None;
    for path in &script_paths {
        if std::path::Path::new(path).exists() {
            script_path = Some(path.to_string());
            break;
        }
    }

    let script_path = match script_path {
        Some(p) => p,
        None => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Tuning script (evolutionary_optimizer.py) not found on system.".to_string())),
    };

    // Parse seed if requested
    let mut seed_arg = None;
    if req.seed {
        if let Ok(cfg) = Config::load_from_db(&db_path) {
            if let Some(bc) = cfg.battery_control {
                if let Some(eh) = bc.evolved_heuristic {
                    seed_arg = Some(format!(
                        "{},{},{},{},{},{},{},{},{}",
                        eh.neg_price_threshold,
                        eh.export_dump_threshold,
                        eh.dump_reserve_demand,
                        eh.dump_reserve_normal,
                        eh.pre_charge_price_threshold,
                        eh.pre_charge_soc_limit,
                        eh.pre_charge_start_hour,
                        if eh.use_adaptive_shaving { 1 } else { 0 },
                        eh.adaptive_safety_buffer
                    ));
                }
            }
        }
    }

    // Spawn child process
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(&script_path)
       .arg("--db").arg(&db_path)
       .arg("--generations").arg(req.generations.to_string())
       .arg("--pop-size").arg(req.population_size.to_string())
       .arg("--penalty").arg(req.cycle_penalty.to_string());

    if let Some(c) = req.cores {
        cmd.arg("--cores").arg(c.to_string());
    }

    // Find MainsMeter name from config source to pass to --mains-source
    let mains_source = if let Ok(cfg) = Config::load_from_db(&db_path) {
        cfg.battery_control.as_ref().and_then(|bc| bc.source.clone()).unwrap_or_else(|| "MainsMeter".to_string())
    } else {
        "MainsMeter".to_string()
    };
    cmd.arg("--mains-source").arg(mains_source);

    if let Some(ref s) = seed_arg {
        cmd.arg("--seed").arg(s);
    }

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to spawn python optimizer: {}", e))),
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Reset progress state
    *progress = TuningProgress {
        is_running: true,
        total_generations: req.generations,
        ..Default::default()
    };

    *get_active_child().lock().unwrap() = Some(child);

    let tx = get_tuning_channel().clone();
    
    // Spawn task to read outputs in background
    tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, BufReader};
        
        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        // Spawn a thread to log stderr
        let tx_err = tx.clone();
        std::thread::spawn(move || {
            for line_res in stderr_reader.lines() {
                if let Ok(line) = line_res {
                    let _ = tx_err.send(TuningLogEvent {
                        percent: 0.0,
                        gen_num: 0,
                        total_gens: 0,
                        best_cost: 0.0,
                        bill: 0.0,
                        cycles: 0.0,
                        log_line: format!("[stderr] {}", line),
                        done: false,
                        error: None,
                        best_params: None,
                    });
                    if let Ok(mut prog) = get_tuning_progress().lock() {
                        prog.logs.push(format!("[stderr] {}", line));
                    }
                }
            }
        });

        let mut in_json = false;
        let mut json_str = String::new();

        for line_res in stdout_reader.lines() {
            if let Ok(line) = line_res {
                let mut percent = 0.0;
                let mut gen_num = 0;
                let mut best_cost = 0.0;
                let mut bill = 0.0;
                let mut cycles = 0.0;
                let mut event_params = None;

                if line.contains("GEN_PROGRESS:") {
                    // GEN_PROGRESS: 45/100 | BEST_COST: 123.45 | BILL: 99.12 | CYCLES: 1.2 | PERCENT: 45.0
                    let parts: Vec<&str> = line.split('|').collect();
                    for part in parts {
                        let p = part.trim();
                        if p.starts_with("GEN_PROGRESS:") {
                            let val_parts: Vec<&str> = p.split_whitespace().collect();
                            if val_parts.len() == 2 {
                                let ratio_parts: Vec<&str> = val_parts[1].split('/').collect();
                                if ratio_parts.len() == 2 {
                                    gen_num = ratio_parts[0].parse().unwrap_or(0);
                                }
                            }
                        } else if p.starts_with("BEST_COST:") {
                            best_cost = p.replace("BEST_COST:", "").trim().parse().unwrap_or(0.0);
                        } else if p.starts_with("BILL:") {
                            bill = p.replace("BILL:", "").trim().parse().unwrap_or(0.0);
                        } else if p.starts_with("CYCLES:") {
                            cycles = p.replace("CYCLES:", "").trim().parse().unwrap_or(0.0);
                        } else if p.starts_with("PERCENT:") {
                            percent = p.replace("PERCENT:", "").trim().parse().unwrap_or(0.0);
                        }
                    }

                    if let Ok(mut prog) = get_tuning_progress().lock() {
                        prog.last_generation = gen_num;
                        prog.percent = percent;
                        prog.best_cost = best_cost;
                        prog.bill = bill;
                        prog.cycles = cycles;
                    }
                }

                if line.contains("--- JSON RESULT ---") {
                    in_json = true;
                    json_str.clear();
                } else if line.contains("-------------------") && in_json {
                    in_json = false;
                    
                    #[derive(serde::Deserialize)]
                    struct BestParamsJson {
                        neg_price_threshold: f64,
                        export_dump_threshold: f64,
                        dump_reserve_demand: f64,
                        dump_reserve_normal: f64,
                        pre_charge_price_threshold: f64,
                        pre_charge_soc_limit: f64,
                        pre_charge_start_hour: u32,
                        use_adaptive_shaving: bool,
                        adaptive_safety_buffer: f64,
                    }

                    #[derive(serde::Deserialize)]
                    #[allow(dead_code)]
                    struct EvolvedTuningResult {
                        status: String,
                        best_params: BestParamsJson,
                    }

                    if let Ok(res) = serde_json::from_str::<EvolvedTuningResult>(&json_str) {
                        let parsed_params = EvolvedHeuristicConfig {
                            neg_price_threshold: res.best_params.neg_price_threshold,
                            export_dump_threshold: res.best_params.export_dump_threshold,
                            dump_reserve_demand: res.best_params.dump_reserve_demand,
                            dump_reserve_normal: res.best_params.dump_reserve_normal,
                            pre_charge_price_threshold: res.best_params.pre_charge_price_threshold,
                            pre_charge_soc_limit: res.best_params.pre_charge_soc_limit,
                            pre_charge_start_hour: res.best_params.pre_charge_start_hour,
                            use_adaptive_shaving: res.best_params.use_adaptive_shaving,
                            adaptive_safety_buffer: res.best_params.adaptive_safety_buffer,
                        };
                        event_params = Some(parsed_params.clone());
                        if let Ok(mut prog) = get_tuning_progress().lock() {
                            prog.best_params = Some(parsed_params);
                        }
                    }
                } else if in_json {
                    json_str.push_str(&line);
                    json_str.push('\n');
                }

                if let Ok(mut prog) = get_tuning_progress().lock() {
                    prog.logs.push(line.clone());
                }

                let _ = tx.send(TuningLogEvent {
                    percent,
                    gen_num,
                    total_gens: req.generations,
                    best_cost,
                    bill,
                    cycles,
                    log_line: line,
                    done: false,
                    error: None,
                    best_params: event_params,
                });
            }
        }

        // Wait for child process exit
        let mut child_lock = get_active_child().lock().unwrap();
        let status = if let Some(mut c) = child_lock.take() {
            c.wait()
        } else {
            Ok(std::process::ExitStatus::default())
        };

        let mut progress_lock = get_tuning_progress().lock().unwrap();
        progress_lock.is_running = false;

        let (done_ok, err_msg) = match status {
            Ok(s) if s.success() => (true, None),
            Ok(s) => (false, Some(format!("Python optimizer script exited with status: {}", s))),
            Err(e) => (false, Some(format!("Failed to await Python script: {}", e))),
        };

        if !done_ok {
            progress_lock.error = err_msg.clone();
        }

        let _ = tx.send(TuningLogEvent {
            percent: progress_lock.percent,
            gen_num: progress_lock.last_generation,
            total_gens: progress_lock.total_generations,
            best_cost: progress_lock.best_cost,
            bill: progress_lock.bill,
            cycles: progress_lock.cycles,
            log_line: if done_ok { "Tuning successfully completed.".to_string() } else { format!("Error: {}", err_msg.as_ref().unwrap()) },
            done: true,
            error: err_msg,
            best_params: progress_lock.best_params.clone(),
        });
    });

    Ok(Json(serde_json::json!({ "status": "started" })))
}

pub async fn handle_cancel_training() -> impl IntoResponse {
    let mut child_lock = get_active_child().lock().unwrap();
    if let Some(mut child) = child_lock.take() {
        let _ = child.kill();
        let _ = child.wait();
    }

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
    });

    Json(serde_json::json!({ "status": "cancelled" }))
}

pub async fn handle_apply_training(
    reload_tx: Sender<()>,
    db_path: String,
    params: EvolvedHeuristicConfig,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mut cfg = match Config::load_from_db(&db_path) {
        Ok(c) => c,
        Err(e) => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to load config: {}", e))),
    };

    if let Some(ref mut bc) = cfg.battery_control {
        bc.evolved_heuristic = Some(params);
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
