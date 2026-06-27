use crate::config::InfluxConfig;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

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
