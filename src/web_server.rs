use crate::config::Config;
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
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct SystemStatus {
    pub active_mode: String,
    pub grid_target: f64,
    pub inverters: HashMap<String, InverterStatus>,
    pub meter_power: f64,
}

pub static SYSTEM_STATUS: OnceLock<Mutex<SystemStatus>> = OnceLock::new();

pub fn get_system_status() -> &'static Mutex<SystemStatus> {
    SYSTEM_STATUS.get_or_init(|| Mutex::new(SystemStatus::default()))
}

pub async fn run_web_server(reload_tx: Sender<()>, db_path: String) {
    let state = Arc::new(reload_tx);
    let db_path_clone = db_path.clone();

    let app = Router::new()
        .route("/", get(serve_dashboard))
        .route("/style.css", get(serve_style))
        .route("/app.js", get(serve_js))
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
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Web UI and REST API running at http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
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
