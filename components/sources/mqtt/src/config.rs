use crate::QualityOfService;
use rumqttc::{MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq)]
pub enum MQTTAuthMethod {
    None,
    UsernamePassword,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MQTTSourceConfig {
    /// MQTT broker host address
    #[serde(default = "default_host")]
    pub host: String,

    /// MQTT broker port
    #[serde(default = "default_port")]
    pub port: u16,

    /// MQTT topic to subscribe to
    #[serde(default = "default_topic")]
    pub topic: String,

    /// Quality of Service level for MQTT messages
    #[serde(default = "default_qos")]
    pub qos: QualityOfService,

    /// Optional username for MQTT authentication
    pub username: Option<String>,

    /// Optional password for MQTT authentication
    pub password: Option<String>,

    /// Authentication method
    #[serde(default = "default_auth_method")]
    pub auth_method: MQTTAuthMethod,

    /// Capacity of the internal channel buffer for incoming messages
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,

    /// Optional timeout in milliseconds for MQTT operations
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Adaptive batching: maximum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_batch_size: Option<usize>,

    /// Adaptive batching: minimum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_batch_size: Option<usize>,

    /// Adaptive batching: maximum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_wait_ms: Option<u64>,

    /// Adaptive batching: minimum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_wait_ms: Option<u64>,

    /// Adaptive batching: throughput window in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_window_secs: Option<u64>,

    /// Whether adaptive batching is enabled
    #[serde(
        default = "default_adaptive_enabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub adaptive_enabled: Option<bool>,
}

pub fn default_host() -> String {
    "localhost".to_string()
}

pub fn default_port() -> u16 {
    1883
}

pub fn default_topic() -> String {
    "mqtt/topic".to_string()
}

pub fn default_qos() -> QualityOfService {
    QualityOfService::ExactlyOnce
}

pub fn default_channel_capacity() -> usize {
    100
}

pub fn default_timeout_ms() -> u64 {
    5000
}

pub fn default_auth_method() -> MQTTAuthMethod {
    MQTTAuthMethod::None
}

pub fn default_adaptive_enabled() -> Option<bool> {
    Some(false)
}

impl MQTTSourceConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.host.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: host cannot be empty. \
                 Please specify a valid host address for the MQTT broker."
            ));
        }
        if self.port == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: port cannot be 0. \
                 Please specify a valid port number for the MQTT broker (1-65535)."
            ));
        }
        if self.topic.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: topic cannot be empty. \
                 Please specify a valid topic for the MQTT broker."
            ));
        }
        if self.timeout_ms == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: timeout_ms cannot be 0. \
                 Please specify a positive timeout value in milliseconds."
            ));
        }

        // Validate adaptive batching settings
        if let (Some(min), Some(max)) = (self.adaptive_min_batch_size, self.adaptive_max_batch_size)
        {
            if min > max {
                return Err(anyhow::anyhow!(
                    "Validation error: adaptive_min_batch_size ({min}) cannot be greater than \
                     adaptive_max_batch_size ({max})"
                ));
            }
        }

        if let (Some(min), Some(max)) = (self.adaptive_min_wait_ms, self.adaptive_max_wait_ms) {
            if min > max {
                return Err(anyhow::anyhow!(
                    "Validation error: adaptive_min_wait_ms ({min}) cannot be greater than \
                     adaptive_max_wait_ms ({max})"
                ));
            }
        }

        Ok(())
    }

    pub fn to_mqtt_connection_config(&self, id: impl Into<String>) -> MQTTConnectionConfig {
        let mut options = MqttOptions::new(id.into(), self.host.clone(), self.port);
        if let Some(username) = &self.username {
            options.set_credentials(username, self.password.as_deref().unwrap_or(""));
        }
        MQTTConnectionConfig {
            options,
            qos: match self.qos {
                QualityOfService::AtMostOnce => QoS::AtMostOnce,
                QualityOfService::AtLeastOnce => QoS::AtLeastOnce,
                QualityOfService::ExactlyOnce => QoS::ExactlyOnce,
            },
            channel_capacity: self.channel_capacity,
            timeout_ms: self.timeout_ms,
            topic: self.topic.clone(),
        }
    }
}

pub struct MQTTConnectionConfig {
    pub options: MqttOptions,
    pub qos: QoS,
    pub channel_capacity: usize,
    pub timeout_ms: u64,
    pub topic: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_config_deserialization_minimal() {
        let yaml = r#"
        host: "localhost"
        port: 1883
        topic: "test/topic"
        "#;
        let config: MQTTSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 1883);
        assert_eq!(config.topic, "test/topic");
        assert_eq!(config.qos, QualityOfService::ExactlyOnce);
        assert_eq!(config.username, None);
        assert_eq!(config.password, None);
        assert_eq!(config.auth_method, MQTTAuthMethod::None);
        assert_eq!(config.channel_capacity, 100);
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.adaptive_enabled, Some(false));
    }

    #[test]
    fn test_config_serialization() {
        let config = MQTTSourceConfig {
            host: "localhost".to_string(),
            port: 1883,
            topic: "test/topic".to_string(),
            qos: QualityOfService::AtLeastOnce,
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            auth_method: MQTTAuthMethod::UsernamePassword,
            channel_capacity: 200,
            timeout_ms: 10000,
            adaptive_enabled: Some(true),
            adaptive_max_batch_size: Some(50),
            adaptive_min_batch_size: Some(10),
            adaptive_max_wait_ms: Some(500),
            adaptive_min_wait_ms: Some(100),
            adaptive_window_secs: Some(60),
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        let deserialized_config: MQTTSourceConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(config, deserialized_config);
    }

    #[test]
    fn test_config_adaptive_config_disabled() {
        let yaml = r#"
        host: "localhost"
        port: 1883
        topic: "test/topic"
        adaptive_enabled: false
        "#;
        let config: MQTTSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.adaptive_enabled, Some(false));
    }

    #[test]
    fn test_config_default_values() {
        let config = MQTTSourceConfig {
            host: default_host(),
            port: default_port(),
            topic: default_topic(),
            qos: default_qos(),
            username: None,
            password: None,
            auth_method: default_auth_method(),
            channel_capacity: default_channel_capacity(),
            timeout_ms: default_timeout_ms(),
            adaptive_enabled: default_adaptive_enabled(),
            adaptive_max_batch_size: None,
            adaptive_min_batch_size: None,
            adaptive_max_wait_ms: None,
            adaptive_min_wait_ms: None,
            adaptive_window_secs: None,
        };

        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 1883);
        assert_eq!(config.topic, "mqtt/topic");
        assert_eq!(config.qos, QualityOfService::ExactlyOnce);
        assert_eq!(config.username, None);
        assert_eq!(config.password, None);
        assert_eq!(config.auth_method, MQTTAuthMethod::None);
        assert_eq!(config.channel_capacity, 100);
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.adaptive_enabled, Some(false));
    }

    #[test]
    fn test_config_port_range() {
        // Test valid port
        let yaml = r#"
        host: "localhost"
        port: 65535
        "#;
        let config: MQTTSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.port, 65535);

        // Test invalid port (0)
        let yaml = r#"
        host: "localhost"
        port: 0
        "#;
        let result: Result<MQTTSourceConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());
        let config = result.unwrap();
        let result = config.validate();
        assert!(result.is_err());

        // Test invalid port (> 65535 cannot be represented by u16, deserialization should fail)
        let yaml = r#"
        host: "localhost"
        port: 70000
        "#;
        let result: Result<MQTTSourceConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
    }
}
