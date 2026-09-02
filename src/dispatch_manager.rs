use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryBatch {
    pub device_name: String,
    pub timestamp: i64,
    pub metrics: HashMap<String, f64>,
}

#[derive(Debug, Clone)]
pub enum DriverCommand {
    SetChargeRate { watts: i32 },
    SetDischargeRate { watts: i32 },
    SetMode { mode: String },
    SetGridTarget { watts: f64 },
}

pub struct DispatchManager {
    rx_driver_telemetry: mpsc::Receiver<TelemetryBatch>,
    forwarder_senders: Vec<mpsc::Sender<TelemetryBatch>>,
    power_manager_sender: mpsc::Sender<TelemetryBatch>,
}

impl DispatchManager {
    pub fn new(
        rx_driver_telemetry: mpsc::Receiver<TelemetryBatch>,
        forwarder_senders: Vec<mpsc::Sender<TelemetryBatch>>,
        power_manager_sender: mpsc::Sender<TelemetryBatch>,
    ) -> Self {
        DispatchManager {
            rx_driver_telemetry,
            forwarder_senders,
            power_manager_sender,
        }
    }

    pub async fn run(mut self, cancel_token: CancellationToken) {
        println!("DispatchManager running and routing queues...");
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    println!("DispatchManager shutting down...");
                    break;
                }
                Some(batch) = self.rx_driver_telemetry.recv() => {
                    self.process_batch(batch).await;
                }
            }
        }
    }

    async fn process_batch(&self, batch: TelemetryBatch) {
        // 1. Forward ALL telemetry metrics to all enabled forwarders (MQTT, EmonCMS, Influx)
        for sender in &self.forwarder_senders {
            let _ = sender.try_send(batch.clone());
        }

        // 2. Buffer all telemetry in memory for SQLite telemetry history database
        let now_ts = batch.timestamp;
        let new_recs: Vec<crate::power_manager::HistoryRecord> = batch
            .metrics
            .iter()
            .map(|(metric_name, &val)| {
                let topic = format!("{}/{}", batch.device_name, metric_name);
                crate::power_manager::HistoryRecord {
                    timestamp: now_ts,
                    topic,
                    value: val,
                }
            })
            .collect();
        crate::database::push_pending_history_records(new_recs);

        // 3. Update live meter status for web details view
        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
            let meter_status = status.meters.entry(batch.device_name.clone()).or_default();
            meter_status.last_updated = Some(now_ts as u64);
            for (k, &v) in &batch.metrics {
                meter_status.raw_metrics.insert(k.clone(), format!("{:.3}", v));
            }
            let lower = batch.device_name.to_lowercase();
            if lower.contains("sdm") || lower.contains("mains") {
                meter_status.driver_type = Some("SDM630Modbusv2".to_string());
            } else if lower.contains("dtsu") {
                meter_status.driver_type = Some("DTSU666".to_string());
            } else {
                meter_status.driver_type = Some("MQTTPowerMeter".to_string());
            }
        }

        // 4. Filter useful control information for PowerManager using exact metric name matching
        let mut useful_metrics = HashMap::new();
        for (metric, val) in &batch.metrics {
            match metric.as_str() {
                "Total system power"
                | "Phase 1 power"
                | "Phase 2 power"
                | "Phase 3 power"
                | "Battery Capacity"
                | "Battery Power"
                | "PV1 Power"
                | "PV2 Power"
                | "Measured Power"
                | "BMS Max Charge Current"
                | "Battery Voltage"
                | "command/mode"
                | "command/grid_target" => {
                    useful_metrics.insert(metric.clone(), *val);
                }
                _ => {}
            }
        }

        if !useful_metrics.is_empty() {
            let filtered_batch = TelemetryBatch {
                device_name: batch.device_name.clone(),
                timestamp: batch.timestamp,
                metrics: useful_metrics,
            };
            if let Err(e) = self.power_manager_sender.try_send(filtered_batch) {
                eprintln!("DispatchManager: WARNING: Telemetry queue to PowerManager full: {}", e);
            }
        }
    }
}
