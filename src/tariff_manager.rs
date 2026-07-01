use crate::config::{TariffConfig, TouTariffPeriod};
use chrono::{Local, NaiveTime, Timelike};
use std::sync::{Arc, RwLock};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, Default)]
pub struct CurrentTariffRates {
    pub import_rate: f64, // cents/kWh
    pub export_rate: f64, // cents/kWh
}

pub struct TariffManager {
    config: Option<TariffConfig>,
    current_rates: Arc<RwLock<CurrentTariffRates>>,
}

impl TariffManager {
    pub fn new(config: Option<TariffConfig>) -> Self {
        let initial_rates = match &config {
            Some(TariffConfig::Flat {
                import_rate,
                export_rate,
            }) => CurrentTariffRates {
                import_rate: *import_rate,
                export_rate: *export_rate,
            },
            _ => CurrentTariffRates::default(),
        };

        TariffManager {
            config,
            current_rates: Arc::new(RwLock::new(initial_rates)),
        }
    }

    pub fn get_current_rates(&self) -> CurrentTariffRates {
        match &self.config {
            Some(TariffConfig::Flat { .. }) => *self.current_rates.read().unwrap(),
            Some(TariffConfig::Tou { periods }) => Self::lookup_tou_rate(periods),
            Some(TariffConfig::Amber { .. }) => *self.current_rates.read().unwrap(),
            None => CurrentTariffRates::default(),
        }
    }

    pub fn config(&self) -> Option<&TariffConfig> {
        self.config.as_ref()
    }

    pub fn set_current_rates(&self, rates: CurrentTariffRates) {
        let mut r = self.current_rates.write().unwrap();
        *r = rates;
    }

    fn lookup_tou_rate(periods: &[TouTariffPeriod]) -> CurrentTariffRates {
        let now_time = Local::now().time();
        let now =
            match NaiveTime::from_hms_opt(now_time.hour(), now_time.minute(), now_time.second()) {
                Some(t) => t,
                None => return CurrentTariffRates::default(),
            };
        Self::lookup_tou_rate_at(periods, now)
    }

    fn lookup_tou_rate_at(periods: &[TouTariffPeriod], now: NaiveTime) -> CurrentTariffRates {
        for period in periods {
            let start = match parse_time(&period.start) {
                Some(t) => t,
                None => continue,
            };
            let end = match parse_time(&period.end) {
                Some(t) => t,
                None => continue,
            };

            let matches = if start < end {
                now >= start && now < end
            } else {
                !(now >= end && now < start)
            };

            if matches {
                return CurrentTariffRates {
                    import_rate: period.import_rate,
                    export_rate: period.export_rate,
                };
            }
        }
        CurrentTariffRates::default()
    }

    pub async fn start_background_loop(&self, cancel_token: CancellationToken) {
        if let Some(TariffConfig::Amber {
            api_key,
            site_id,
            api_url,
            ..
        }) = &self.config
        {
            let base_url = api_url
                .clone()
                .unwrap_or_else(|| "https://api.amber.com.au".to_string());
            let client = reqwest::Client::new();
            let url = format!(
                "{}/v1/sites/{}/prices/current?resolution=5",
                base_url.trim_end_matches('/'),
                site_id
            );
            let current_rates_clone = self.current_rates.clone();

            // Run immediate update
            if let Err(e) =
                Self::fetch_amber_prices(&client, &url, api_key, &current_rates_clone).await
            {
                println!("Error initializing Amber prices: {}", e);
            }

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    _ = sleep(Duration::from_secs(60)) => {
                        if let Err(e) = Self::fetch_amber_prices(&client, &url, api_key, &current_rates_clone).await {
                            println!("Error polling Amber prices: {}", e);
                            // Retain last known price in case of failure
                        }
                    }
                }
            }
        }
    }

    async fn fetch_amber_prices(
        client: &reqwest::Client,
        url: &str,
        api_key: &str,
        current_rates: &Arc<RwLock<CurrentTariffRates>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AmberPrice {
            channel_type: String,
            per_kwh: f64,
        }

        let resp = client
            .get(url)
            .bearer_auth(api_key)
            .header("Accept", "application/json")
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(format!("HTTP error status: {}", resp.status()).into());
        }

        let prices: Vec<AmberPrice> = resp.json().await?;
        let mut import_rate = 0.0;
        let mut export_rate = 0.0;

        for price in prices {
            if price.channel_type == "general" {
                import_rate = price.per_kwh; // already in cents/kWh
            } else if price.channel_type == "feedIn" {
                export_rate = price.per_kwh; // already in cents/kWh
            }
        }

        let mut rates = current_rates.write().unwrap();
        rates.import_rate = import_rate;
        rates.export_rate = export_rate;
        println!(
            "Amber prices updated: import = {} c/kWh, export = {} c/kWh",
            import_rate, export_rate
        );
        if let Ok(mut status) = crate::web_server::get_system_status().lock() {
            status.import_price = Some(import_rate);
            status.export_price = Some(export_rate);
        }
        Ok(())
    }
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(s, "%k:%M:%S"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%I:%M:%S %p"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%k:%M"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_tou_rate() {
        let periods = vec![
            TouTariffPeriod {
                name: "Peak".to_string(),
                start: "14:00:00".to_string(),
                end: "20:00:00".to_string(),
                import_rate: 35.0,
                export_rate: 10.0,
            },
            TouTariffPeriod {
                name: "Off-Peak".to_string(),
                start: "20:00:00".to_string(),
                end: "14:00:00".to_string(),
                import_rate: 15.0,
                export_rate: 5.0,
            },
        ];

        // Test normal time window
        let test_time = NaiveTime::from_hms_opt(16, 0, 0).unwrap();
        let rates = TariffManager::lookup_tou_rate_at(&periods, test_time);
        assert_eq!(rates.import_rate, 35.0);
        assert_eq!(rates.export_rate, 10.0);

        // Test overnight window
        let test_time_night = NaiveTime::from_hms_opt(22, 0, 0).unwrap();
        let rates_night = TariffManager::lookup_tou_rate_at(&periods, test_time_night);
        assert_eq!(rates_night.import_rate, 15.0);
        assert_eq!(rates_night.export_rate, 5.0);

        // Test verify parse_time helper
        assert_eq!(
            parse_time("14:00:00").unwrap(),
            NaiveTime::from_hms_opt(14, 0, 0).unwrap()
        );
        assert_eq!(
            parse_time("02:00:00 PM").unwrap(),
            NaiveTime::from_hms_opt(14, 0, 0).unwrap()
        );
    }

    #[test]
    fn test_flat_tariff_manager() {
        let config = TariffConfig::Flat {
            import_rate: 28.5,
            export_rate: 10.2,
        };
        let tm = TariffManager::new(Some(config));
        let rates = tm.get_current_rates();
        assert_eq!(rates.import_rate, 28.5);
        assert_eq!(rates.export_rate, 10.2);
    }

    #[test]
    fn test_none_tariff_manager() {
        let tm = TariffManager::new(None);
        let rates = tm.get_current_rates();
        assert_eq!(rates.import_rate, 0.0);
        assert_eq!(rates.export_rate, 0.0);
    }

    #[tokio::test]
    async fn test_background_loop_cancellation() {
        let config = TariffConfig::Amber {
            api_key: "key".to_string(),
            site_id: "site".to_string(),
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            api_url: Some("http://127.0.0.1:9999".to_string()),
        };
        let tm = TariffManager::new(Some(config));
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        tm.start_background_loop(cancel_token).await;
    }

    #[tokio::test]
    async fn test_fetch_amber_prices() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let json_body = r#"[
                {"channelType": "general", "perKwh": 35.4},
                {"channelType": "feedIn", "perKwh": 8.2}
            ]"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                json_body.len(),
                json_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!(
            "http://127.0.0.1:{}/v1/sites/test-site/prices/current?resolution=5",
            port
        );
        let rates = Arc::new(RwLock::new(CurrentTariffRates::default()));

        TariffManager::fetch_amber_prices(&client, &url, "test-key", &rates)
            .await
            .unwrap();

        let current = *rates.read().unwrap();
        assert!((current.import_rate - 35.4).abs() < 1e-5);
        assert!((current.export_rate - 8.2).abs() < 1e-5);

        server_task.await.unwrap();
    }

    #[test]
    fn test_tariff_manager_extra_coverage() {
        // Test config() getter
        let config = TariffConfig::Flat {
            import_rate: 20.0,
            export_rate: 5.0,
        };
        let tm = TariffManager::new(Some(config.clone()));
        assert!(tm.config().is_some());

        // Test set_current_rates()
        tm.set_current_rates(CurrentTariffRates {
            import_rate: 12.3,
            export_rate: 4.5,
        });
        let current = tm.get_current_rates();
        assert_eq!(current.import_rate, 12.3);
        assert_eq!(current.export_rate, 4.5);

        // Test TOU get_current_rates
        let tou_config = TariffConfig::Tou {
            periods: vec![TouTariffPeriod {
                name: "Always".to_string(),
                start: "00:00:00".to_string(),
                end: "23:59:59".to_string(),
                import_rate: 10.0,
                export_rate: 2.0,
            }],
        };
        let tm_tou = TariffManager::new(Some(tou_config));
        let tou_rates = tm_tou.get_current_rates();
        assert_eq!(tou_rates.import_rate, 10.0);
        assert_eq!(tou_rates.export_rate, 2.0);

        // Test parse_time fallback returns None
        assert!(parse_time("invalid-time-format").is_none());

        // Test lookup_tou_rate_at with invalid start/end parse
        let bad_periods = vec![TouTariffPeriod {
            name: "Bad".to_string(),
            start: "bad_start".to_string(),
            end: "bad_end".to_string(),
            import_rate: 10.0,
            export_rate: 2.0,
        }];
        let bad_rates = TariffManager::lookup_tou_rate_at(&bad_periods, NaiveTime::from_hms_opt(12, 0, 0).unwrap());
        assert_eq!(bad_rates.import_rate, 0.0);
    }

    #[tokio::test]
    async fn test_fetch_amber_prices_errors() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}/v1/sites/test-site/prices/current", port);
        let rates = Arc::new(RwLock::new(CurrentTariffRates::default()));

        let res = TariffManager::fetch_amber_prices(&client, &url, "test-key", &rates).await;
        assert!(res.is_err());

        server_task.await.unwrap();
    }
}
