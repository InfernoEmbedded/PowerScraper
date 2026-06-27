use crate::config::EmonCMSConfig;
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub async fn forward_to_emoncms(
    client: &reqwest::Client,
    config: &EmonCMSConfig,
    device_name: &str,
    metrics: &HashMap<String, String>,
    cancel_token: &CancellationToken,
) {
    let mut payload = metrics.clone();
    payload.remove("Serial");
    payload.remove("name");

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
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_token_emon.cancelled() => {}
                res = client
                    .get(&emon_url)
                    .query(&query_params)
                    .timeout(Duration::from_secs_f64(timeout_sec))
                    .send() => {
                        match res {
                            Ok(resp) => {
                                if !resp.status().is_success() {
                                    println!(
                                        "EmonCMS forward failed with status: {}",
                                        resp.status()
                                    );
                                }
                            }
                            Err(e) => {
                                println!("EmonCMS request error: {}", e);
                            }
                        }
                    }
            }
        });
    }
}
