use crate::config::EmonCMSConfig;
use crate::dispatch_manager::TelemetryBatch;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct EmonCMSForwarder {
    config: EmonCMSConfig,
    rx_input: mpsc::Receiver<TelemetryBatch>,
}

impl EmonCMSForwarder {
    pub fn new(config: EmonCMSConfig, rx_input: mpsc::Receiver<TelemetryBatch>) -> Self {
        EmonCMSForwarder { config, rx_input }
    }

    pub async fn run(mut self, cancel_token: CancellationToken) {
        let client = reqwest::Client::new();
        println!("EmonCMS Forwarder running...");
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    println!("EmonCMS Forwarder shutting down...");
                    break;
                }
                Some(batch) = self.rx_input.recv() => {
                    let mut string_map = HashMap::new();
                    for (k, v) in batch.metrics {
                        string_map.insert(k, v.to_string());
                    }
                    forward_to_emoncms(&client, &self.config, &batch.device_name, &string_map, &cancel_token).await;
                }
            }
        }
    }
}

pub async fn forward_to_emoncms(
    client: &reqwest::Client,
    config: &EmonCMSConfig,
    device_name: &str,
    metrics: &HashMap<String, String>,
    cancel_token: &CancellationToken,
) {
    let mut payload = HashMap::new();
    for (k, v) in metrics.iter() {
        if k == "Serial" || k == "name" {
            continue;
        }
        let json_val = if let Ok(i) = v.parse::<i64>() {
            serde_json::Value::Number(i.into())
        } else if let Ok(f) = v.parse::<f64>() {
            if let Some(num) = serde_json::Number::from_f64(f) {
                serde_json::Value::Number(num)
            } else {
                serde_json::Value::String(v.clone())
            }
        } else if let Ok(b) = v.parse::<bool>() {
            serde_json::Value::Bool(b)
        } else {
            serde_json::Value::String(v.clone())
        };
        payload.insert(k.clone(), json_val);
    }

    let url = format!("{}/input/post", config.server);
    let mut query_params = HashMap::new();
    query_params.insert("apikey", config.api_key.clone());
    query_params.insert("node", device_name.to_string());
    if let Ok(json_str) = serde_json::to_string(&payload) {
        query_params.insert("fulljson", json_str);

        let emon_url = url.clone();
        let client = client.clone();
        let timeout_sec = config.timeout;
        let cancel_token_emon = cancel_token.clone();
        let device_name_log = device_name.to_string();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_token_emon.cancelled() => {}
                res = client
                    .post(&emon_url)
                    .form(&query_params)
                    .timeout(Duration::from_secs_f64(timeout_sec))
                    .send() => {
                        match res {
                            Ok(resp) => {
                                let status = resp.status();
                                let body = resp.text().await.unwrap_or_default();
                                eprintln!("[EmonCMS Log] node={}: status={}, body={}", device_name_log, status, body);
                            }
                            Err(e) => {
                                eprintln!("[EmonCMS Log] node={} error: {}", device_name_log, e);
                            }
                        }
                    }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_forward_to_emoncms_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let cancel_token = CancellationToken::new();
        let config = EmonCMSConfig {
            server: format!("http://127.0.0.1:{}", port),
            api_key: "test_key".to_string(),
            timeout: 2.0,
        };

        let client = reqwest::Client::new();
        let mut metrics = HashMap::new();
        metrics.insert("power".to_string(), "100".to_string());

        let server_task = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        forward_to_emoncms(&client, &config, "solax1", &metrics, &cancel_token).await;
        let _ = server_task.await;
    }
}
