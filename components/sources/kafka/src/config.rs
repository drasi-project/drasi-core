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

use drasi_lib::sources::SourceBaseParams;
use drasi_lib::DispatchMode;
use drasi_source_mapping::SourceMapping;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for the Kafka source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KafkaSourceConfig {
    /// Source ID
    pub id: String,

    /// Kafka bootstrap servers (comma-separated)
    pub bootstrap_servers: String,

    /// Kafka topic to consume
    pub topic: String,

    /// Consumer group ID
    pub group_id: String,

    /// Default node label (used in default mapping and tombstone handling)
    pub node_label: String,

    /// Custom mappings (if None, uses default mapping: key→ID, payload→properties)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mappings: Option<Vec<SourceMapping>>,

    /// Security protocol (PLAINTEXT, SASL_PLAINTEXT, SASL_SSL, SSL)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_protocol: Option<String>,

    /// SASL mechanism (PLAIN, SCRAM-SHA-256, SCRAM-SHA-512)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sasl_mechanism: Option<String>,

    /// SASL username
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sasl_username: Option<String>,

    /// SASL password
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sasl_password: Option<String>,

    /// Initial offset behavior when no resume position exists
    #[serde(default)]
    pub auto_offset_reset: AutoOffsetReset,

    /// Additional rdkafka configuration properties (escape hatch)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub additional_properties: HashMap<String, String>,
}

impl KafkaSourceConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.bootstrap_servers.is_empty() {
            return Err(anyhow::anyhow!("bootstrap_servers cannot be empty"));
        }
        if self.topic.is_empty() {
            return Err(anyhow::anyhow!("topic cannot be empty"));
        }
        if self.group_id.is_empty() {
            return Err(anyhow::anyhow!("group_id cannot be empty"));
        }
        if self.node_label.is_empty() {
            return Err(anyhow::anyhow!("node_label cannot be empty"));
        }

        if let Some(ref mappings) = self.mappings {
            for (idx, mapping) in mappings.iter().enumerate() {
                mapping
                    .validate()
                    .map_err(|e| anyhow::anyhow!("mappings[{idx}]: {e}"))?;
            }
        }

        Ok(())
    }
}

/// Auto offset reset behavior.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoOffsetReset {
    /// Start from the beginning of the topic
    #[default]
    Earliest,
    /// Start from the end of the topic (only new messages)
    Latest,
}

/// Builder for constructing a KafkaSource.
pub struct KafkaSourceBuilder {
    id: String,
    bootstrap_servers: Option<String>,
    topic: Option<String>,
    group_id: Option<String>,
    node_label: Option<String>,
    mappings: Option<Vec<SourceMapping>>,
    security_protocol: Option<String>,
    sasl_mechanism: Option<String>,
    sasl_username: Option<String>,
    sasl_password: Option<String>,
    auto_offset_reset: AutoOffsetReset,
    additional_properties: HashMap<String, String>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
}

impl KafkaSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            bootstrap_servers: None,
            topic: None,
            group_id: None,
            node_label: None,
            mappings: None,
            security_protocol: None,
            sasl_mechanism: None,
            sasl_username: None,
            sasl_password: None,
            auto_offset_reset: AutoOffsetReset::Earliest,
            additional_properties: HashMap::new(),
            bootstrap_provider: None,
        }
    }

    pub fn bootstrap_servers(mut self, servers: impl Into<String>) -> Self {
        self.bootstrap_servers = Some(servers.into());
        self
    }

    pub fn topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = Some(group_id.into());
        self
    }

    pub fn node_label(mut self, label: impl Into<String>) -> Self {
        self.node_label = Some(label.into());
        self
    }

    pub fn mappings(mut self, mappings: Vec<SourceMapping>) -> Self {
        self.mappings = Some(mappings);
        self
    }

    pub fn security_protocol(mut self, protocol: impl Into<String>) -> Self {
        self.security_protocol = Some(protocol.into());
        self
    }

    pub fn sasl_mechanism(mut self, mechanism: impl Into<String>) -> Self {
        self.sasl_mechanism = Some(mechanism.into());
        self
    }

    pub fn sasl_username(mut self, username: impl Into<String>) -> Self {
        self.sasl_username = Some(username.into());
        self
    }

    pub fn sasl_password(mut self, password: impl Into<String>) -> Self {
        self.sasl_password = Some(password.into());
        self
    }

    pub fn auto_offset_reset(mut self, reset: AutoOffsetReset) -> Self {
        self.auto_offset_reset = reset;
        self
    }

    pub fn additional_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.additional_properties.insert(key.into(), value.into());
        self
    }

    pub fn bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Build the KafkaSource.
    pub fn build(self) -> anyhow::Result<super::KafkaSource> {
        let config = KafkaSourceConfig {
            id: self.id,
            bootstrap_servers: self
                .bootstrap_servers
                .ok_or_else(|| anyhow::anyhow!("bootstrap_servers is required"))?,
            topic: self
                .topic
                .ok_or_else(|| anyhow::anyhow!("topic is required"))?,
            group_id: self
                .group_id
                .ok_or_else(|| anyhow::anyhow!("group_id is required"))?,
            node_label: self
                .node_label
                .ok_or_else(|| anyhow::anyhow!("node_label is required"))?,
            mappings: self.mappings,
            security_protocol: self.security_protocol,
            sasl_mechanism: self.sasl_mechanism,
            sasl_username: self.sasl_username,
            sasl_password: self.sasl_password,
            auto_offset_reset: self.auto_offset_reset,
            additional_properties: self.additional_properties,
        };

        config.validate()?;

        let mut params =
            SourceBaseParams::new(&config.id).with_dispatch_mode(DispatchMode::Channel);
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        let source = super::KafkaSource::with_params(config, params)?;

        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation_valid() {
        let config = KafkaSourceConfig {
            id: "test".to_string(),
            bootstrap_servers: "localhost:9092".to_string(),
            topic: "test-topic".to_string(),
            group_id: "test-group".to_string(),
            node_label: "TestNode".to_string(),
            mappings: None,
            security_protocol: None,
            sasl_mechanism: None,
            sasl_username: None,
            sasl_password: None,
            auto_offset_reset: AutoOffsetReset::Earliest,
            additional_properties: HashMap::new(),
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validation_missing_bootstrap_servers() {
        let config = KafkaSourceConfig {
            id: "test".to_string(),
            bootstrap_servers: "".to_string(),
            topic: "test-topic".to_string(),
            group_id: "test-group".to_string(),
            node_label: "TestNode".to_string(),
            mappings: None,
            security_protocol: None,
            sasl_mechanism: None,
            sasl_username: None,
            sasl_password: None,
            auto_offset_reset: AutoOffsetReset::Earliest,
            additional_properties: HashMap::new(),
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("bootstrap_servers"));
    }

    #[test]
    fn test_config_validation_missing_topic() {
        let config = KafkaSourceConfig {
            id: "test".to_string(),
            bootstrap_servers: "localhost:9092".to_string(),
            topic: "".to_string(),
            group_id: "test-group".to_string(),
            node_label: "TestNode".to_string(),
            mappings: None,
            security_protocol: None,
            sasl_mechanism: None,
            sasl_username: None,
            sasl_password: None,
            auto_offset_reset: AutoOffsetReset::Earliest,
            additional_properties: HashMap::new(),
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("topic"));
    }

    #[test]
    fn test_auto_offset_reset_default() {
        assert_eq!(AutoOffsetReset::default(), AutoOffsetReset::Earliest);
    }

    #[test]
    fn test_config_deserialization() {
        let json = r#"{
            "id": "my-kafka",
            "bootstrap_servers": "broker1:9092,broker2:9092",
            "topic": "orders",
            "group_id": "drasi-orders",
            "node_label": "Order",
            "auto_offset_reset": "latest"
        }"#;

        let config: KafkaSourceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.bootstrap_servers, "broker1:9092,broker2:9092");
        assert_eq!(config.topic, "orders");
        assert_eq!(config.auto_offset_reset, AutoOffsetReset::Latest);
    }
}
