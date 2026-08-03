use crate::config::InfluxConfig;
use crate::dispatch_manager::TelemetryBatch;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct InfluxForwarder {
    config: InfluxConfig,
    rx_input: mpsc::Receiver<TelemetryBatch>,
}

impl InfluxForwarder {
    pub fn new(config: InfluxConfig, rx_input: mpsc::Receiver<TelemetryBatch>) -> Self {
        InfluxForwarder { config, rx_input }
    }

    pub async fn run(mut self, cancel_token: CancellationToken) {
        let client = reqwest::Client::new();
        println!("InfluxDB Forwarder running...");
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    println!("InfluxDB Forwarder shutting down...");
                    break;
                }
                Some(batch) = self.rx_input.recv() => {
                    let mut string_map = HashMap::new();
                    for (k, v) in batch.metrics {
                        string_map.insert(k, v.to_string());
                    }
                    forward_to_influx(&client, &self.config, &batch.device_name, &string_map, &cancel_token).await;
                }
            }
        }
    }
}

pub async fn forward_to_influx(
    client: &reqwest::Client,
    config: &InfluxConfig,
    device_name: &str,
    metrics: &HashMap<String, String>,
    cancel_token: &CancellationToken,
) {
    let mut payload = metrics.clone();
    payload.remove("Serial");
    payload.remove("name");

    // Format as line protocol: solax,inverter=dev_name field1=val1,field2=val2
    let mut fields = Vec::new();
    for (k, v) in payload {
        // sanitize key / value
        let key = k.replace(' ', "_").replace(',', "\\,").replace('=', "\\=");
        if let Ok(num) = v.parse::<f64>() {
            fields.push(format!("{}={}", key, num));
        } else {
            let escaped_val = v.replace('"', "\\\"");
            fields.push(format!("{}=\"{}\"", key, escaped_val));
        }
    }

    if !fields.is_empty() {
        let line = format!("solax,inverter={} {}", device_name, fields.join(","));
        let write_url = format!(
            "{}/api/v2/write?org=-&bucket={}/{}",
            config.influx_url,
            config.influx_database,
            config.influx_retention_policy
        );
        let token = format!("{}:{}", config.influx_user, config.influx_pass);

        let client = client.clone();
        let cancel_token_influx = cancel_token.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_token_influx.cancelled() => {}
                res = client
                    .post(&write_url)
                    .header("Authorization", format!("Token {}", token))
                    .body(line)
                    .send() => {
                        match res {
                            Ok(resp) => {
                                if !resp.status().is_success() {
                                    let text = resp.text().await.unwrap_or_default();
                                    println!(
                                        "InfluxDB forward failed: {} - {}",
                                        text, write_url
                                    );
                                }
                            }
                            Err(e) => {
                                println!("InfluxDB request error: {}", e);
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
    use std::time::Duration;

    #[tokio::test]
    async fn test_forward_to_influx_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let config = InfluxConfig {
            influx_url: format!("http://127.0.0.1:{}", port),
            influx_database: "testdb".to_string(),
            influx_measurement: "m".to_string(),
            influx_user: "user".to_string(),
            influx_pass: "pass".to_string(),
            influx_retention_policy: "autogen".to_string(),
        };
        let mut metrics = HashMap::new();
        metrics.insert("PV1 Power".to_string(), "1200.5".to_string());
        metrics.insert("Status".to_string(), "Normal".to_string());

        let cancel = CancellationToken::new();
        forward_to_influx(&client, &config, "solax1", &metrics, &cancel).await;

        // Wait briefly for background task
        tokio::time::sleep(Duration::from_millis(50)).await;
        server_task.await.unwrap();
    }
}

