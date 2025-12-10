// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Configuration for the HTTP source.
//!
//! The HTTP source receives data changes via HTTP endpoints.

use serde::{Deserialize, Serialize};

/// HTTP source configuration
///
/// This config only contains HTTP-specific settings.
/// Bootstrap provider configuration (database, user, password, tables, etc.)
/// should be provided via the source's generic properties map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpSourceConfig {
    /// HTTP host
    pub host: String,

    /// HTTP port
    pub port: u16,

    /// Optional endpoint path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Request timeout in milliseconds
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_enabled: Option<bool>,
}

fn default_timeout_ms() -> u64 {
    10000
}

impl HttpSourceConfig {
    /// Validate the configuration and return an error if invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Port is 0 (invalid port)
    /// - Timeout is 0 (would cause immediate timeouts)
    /// - Adaptive batching min values exceed max values
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.port == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: port cannot be 0. \
                 Please specify a valid port number (1-65535)"
            ));
        }

        if self.timeout_ms == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: timeout_ms cannot be 0. \
                 Please specify a positive timeout value in milliseconds"
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_deserialization_minimal() {
        let yaml = r#"
host: "localhost"
port: 8080
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 8080);
        assert_eq!(config.endpoint, None);
        assert_eq!(config.timeout_ms, 10000); // default
        assert_eq!(config.adaptive_enabled, None);
    }

    #[test]
    fn test_config_deserialization_full() {
        let yaml = r#"
host: "0.0.0.0"
port: 9000
endpoint: "/events"
timeout_ms: 5000
adaptive_max_batch_size: 1000
adaptive_min_batch_size: 10
adaptive_max_wait_ms: 500
adaptive_min_wait_ms: 10
adaptive_window_secs: 60
adaptive_enabled: true
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 9000);
        assert_eq!(config.endpoint, Some("/events".to_string()));
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.adaptive_max_batch_size, Some(1000));
        assert_eq!(config.adaptive_min_batch_size, Some(10));
        assert_eq!(config.adaptive_max_wait_ms, Some(500));
        assert_eq!(config.adaptive_min_wait_ms, Some(10));
        assert_eq!(config.adaptive_window_secs, Some(60));
        assert_eq!(config.adaptive_enabled, Some(true));
    }

    #[test]
    fn test_config_serialization() {
        let config = HttpSourceConfig {
            host: "localhost".to_string(),
            port: 8080,
            endpoint: Some("/data".to_string()),
            timeout_ms: 15000,
            adaptive_max_batch_size: Some(500),
            adaptive_min_batch_size: Some(5),
            adaptive_max_wait_ms: Some(1000),
            adaptive_min_wait_ms: Some(50),
            adaptive_window_secs: Some(30),
            adaptive_enabled: Some(false),
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        let deserialized: HttpSourceConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_adaptive_batching_disabled() {
        let yaml = r#"
host: "localhost"
port: 8080
adaptive_enabled: false
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.adaptive_enabled, Some(false));
    }

    #[test]
    fn test_config_default_values() {
        let config = HttpSourceConfig {
            host: "localhost".to_string(),
            port: 8080,
            endpoint: None,
            timeout_ms: default_timeout_ms(),
            adaptive_max_batch_size: None,
            adaptive_min_batch_size: None,
            adaptive_max_wait_ms: None,
            adaptive_min_wait_ms: None,
            adaptive_window_secs: None,
            adaptive_enabled: None,
        };

        assert_eq!(config.timeout_ms, 10000);
    }

    #[test]
    fn test_config_port_range() {
        // Test valid port
        let yaml = r#"
host: "localhost"
port: 65535
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.port, 65535);

        // Test minimum port
        let yaml = r#"
host: "localhost"
port: 1
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.port, 1);
    }
}
