use crate::config::MqttBrokerConfig;
use rumqttc::{AsyncClient, EventLoop, MqttOptions};
use std::time::Duration;

pub fn create_mqtt_client(client_id: &str, config: &MqttBrokerConfig) -> (AsyncClient, EventLoop) {
    let port = config.port.unwrap_or(1883);
    let mut options = MqttOptions::new(client_id, &config.broker, port);
    options.set_keep_alive(Duration::from_secs(60));
    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        if !username.is_empty() {
            options.set_credentials(username, password);
        }
    }
    AsyncClient::new(options, 100)
}
