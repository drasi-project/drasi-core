#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! MQTT reaction plugin for Drasi
//!
//! This crate publishes continuous query result changes to an MQTT broker.
//! It supports direct configuration through [`MqttReactionConfig`] and fluent
//! construction through [`MqttReactionBuilder`].
//!
//! ## Instance-based Usage
//!
//! ```rust,ignore
//! use drasi_reaction_mqtt::{MqttReaction, MqttReactionConfig};
//! use drasi_reaction_mqtt::config::{MqttAuthMode, MqttTransportMode};
//! use drasi_lib::reactions::common::AdaptiveBatchConfig;
//!
//! // Create configuration
//! let config = MqttReactionConfig {
//!     broker_addr: "mqtt.example.com".to_string(),
//!     port: 1883,
//!     transport_mode: MqttTransportMode::TCP,
//!     keep_alive: 60,
//!     clean_session: true,
//!     auth_mode: MqttAuthMode::UsernamePassword {
//!         username: "user".to_string(),
//!         password: "pass".to_string(),
//!     },
//!     request_channel_capacity: 20,
//!     event_channel_capacity: 40,
//!     pending_throttle: 100,
//!     connection_timeout: 5,
//!     max_packet_size: 2 * 1024,
//!     max_inflight: 30,
//!     default_topic: "test/topic".to_string(),
//!     query_configs: Default::default(),
//!     adaptive: Default::default(),
//! };
//!
//! let reaction = MqttReaction::new(
//!     "my-mqtt-reaction",
//!     vec!["query1".to_string()],
//!     config,
//! );
//! ```

mod adaptive_batcher;
mod client;
pub mod config;
mod mqtt;
mod processor;

use config::{
    default_broker_addr, default_clean_session, default_connection_timeout,
    default_event_channel_capacity, default_inflight, default_keep_alive, default_max_packet_size,
    default_pending_throttle, default_port, default_request_channel_capacity, default_topic,
    MqttAuthMode, MqttCallSpec, MqttQueryConfig, MqttReactionConfig, MqttTransportMode,
};
use drasi_lib::reactions::common::AdaptiveBatchConfig;
pub use mqtt::MqttReaction;
use std::collections::HashMap;

/// Builder for MQTT reaction
///
/// Creates an MqttReaction instance.
///
/// # Example
/// ```rust,ignore
/// use drasi_reaction_mqtt::{MqttReaction, config::MqttAuthMode};
///
/// let reaction = MqttReaction::builder("my-mqtt-reaction")
///     .with_query("query1")
///     .with_broker_addr("mqtt.example.com")
///     .with_port(1883)
///     .with_default_topic("my/default/topic")
///     .with_keep_alive(60)
///     .with_auth_mode(MqttAuthMode::UsernamePassword {
///         username: "user".to_string(),
///         password: "pass".to_string(),
///     })
///     .build()?;
/// ```
pub struct MqttReactionBuilder {
    id: String,
    queries: Vec<String>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,

    broker_addr: String,
    port: u16,
    transport_mode: MqttTransportMode,
    keep_alive: u64,
    clean_session: bool,
    auth_mode: MqttAuthMode,
    request_channel_capacity: usize,
    event_channel_capacity: usize,
    pending_throttle: u64,
    connection_timeout: u64,
    max_packet_size: u32,
    max_inflight: u16,
    default_topic: String,
    query_configs: HashMap<String, MqttQueryConfig>,

    adaptive_config: Option<AdaptiveBatchConfig>,
}

impl MqttReactionBuilder {
    // Create a new MQTT reaction builder with given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            priority_queue_capacity: None,
            auto_start: true,

            broker_addr: default_broker_addr(),
            port: default_port(),
            transport_mode: MqttTransportMode::default(),
            keep_alive: default_keep_alive(),
            clean_session: default_clean_session(),
            auth_mode: MqttAuthMode::default(),
            request_channel_capacity: default_request_channel_capacity(),
            event_channel_capacity: default_event_channel_capacity(),
            pending_throttle: default_pending_throttle(),
            connection_timeout: default_connection_timeout(),
            max_packet_size: default_max_packet_size(),
            max_inflight: default_inflight(),
            default_topic: default_topic(),
            query_configs: HashMap::new(),

            adaptive_config: None,
        }
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_broker_addr(mut self, broker_addr: impl Into<String>) -> Self {
        self.broker_addr = broker_addr.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_transport_mode(mut self, transport_mode: MqttTransportMode) -> Self {
        self.transport_mode = transport_mode;
        self
    }

    pub fn with_keep_alive(mut self, keep_alive: u64) -> Self {
        self.keep_alive = keep_alive;
        self
    }

    pub fn with_clean_session(mut self, clean_session: bool) -> Self {
        self.clean_session = clean_session;
        self
    }

    pub fn with_auth_mode(mut self, auth_mode: MqttAuthMode) -> Self {
        self.auth_mode = auth_mode;
        self
    }

    pub fn with_request_channel_capacity(mut self, capacity: usize) -> Self {
        self.request_channel_capacity = capacity;
        self
    }

    pub fn with_event_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity;
        self
    }

    pub fn with_pending_throttle(mut self, throttle_ms: u64) -> Self {
        self.pending_throttle = throttle_ms;
        self
    }

    pub fn with_connection_timeout(mut self, timeout: u64) -> Self {
        self.connection_timeout = timeout;
        self
    }

    pub fn with_max_packet_size(mut self, size: u32) -> Self {
        self.max_packet_size = size;
        self
    }

    pub fn with_max_inflight(mut self, max_inflight: u16) -> Self {
        self.max_inflight = max_inflight;
        self
    }

    pub fn with_default_topic(mut self, default_topic: impl Into<String>) -> Self {
        self.default_topic = default_topic.into();
        self
    }

    pub fn with_query_configs(mut self, configs: HashMap<String, MqttQueryConfig>) -> Self {
        self.query_configs = configs;
        self
    }

    pub fn with_query_config(
        mut self,
        query_name: impl Into<String>,
        config: MqttQueryConfig,
    ) -> Self {
        self.query_configs.insert(query_name.into(), config);
        self
    }

    pub fn with_query_configs_extend(
        mut self,
        configs: impl IntoIterator<Item = (String, MqttQueryConfig)>,
    ) -> Self {
        self.query_configs.extend(configs);
        self
    }

    // Set the query IDs to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
        self
    }

    /// Adaptive configuration setters
    pub fn with_adaptive_config(mut self, adaptive_config: AdaptiveBatchConfig) -> Self {
        self.adaptive_config = Some(adaptive_config);
        self
    }

    pub fn with_min_adaptive_batch_size(mut self, size: usize) -> Self {
        if let Some(ref mut config) = self.adaptive_config {
            config.adaptive_min_batch_size = size;
        } else {
            self.adaptive_config = Some(AdaptiveBatchConfig {
                adaptive_min_batch_size: size,
                ..Default::default()
            });
        }
        self
    }

    pub fn with_max_adaptive_batch_size(mut self, size: usize) -> Self {
        if let Some(ref mut config) = self.adaptive_config {
            config.adaptive_max_batch_size = size;
        } else {
            self.adaptive_config = Some(AdaptiveBatchConfig {
                adaptive_max_batch_size: size,
                ..Default::default()
            });
        }
        self
    }

    pub fn with_adaptive_window_size(mut self, size: usize) -> Self {
        if let Some(ref mut config) = self.adaptive_config {
            config.adaptive_window_size = size;
        } else {
            self.adaptive_config = Some(AdaptiveBatchConfig {
                adaptive_window_size: size,
                ..Default::default()
            });
        }
        self
    }

    pub fn with_adaptive_batch_timeout_ms(mut self, timeout_ms: u64) -> Self {
        if let Some(ref mut config) = self.adaptive_config {
            config.adaptive_batch_timeout_ms = timeout_ms;
        } else {
            self.adaptive_config = Some(AdaptiveBatchConfig {
                adaptive_batch_timeout_ms: timeout_ms,
                ..Default::default()
            });
        }
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: MqttReactionConfig) -> Self {
        self.broker_addr = config.broker_addr;
        self.port = config.port;
        self.transport_mode = config.transport_mode;
        self.keep_alive = config.keep_alive;
        self.clean_session = config.clean_session;
        self.auth_mode = config.auth_mode;
        self.request_channel_capacity = config.request_channel_capacity;
        self.event_channel_capacity = config.event_channel_capacity;
        self.pending_throttle = config.pending_throttle;
        self.connection_timeout = config.connection_timeout;
        self.max_packet_size = config.max_packet_size;
        self.max_inflight = config.max_inflight;
        self.default_topic = config.default_topic;
        self.query_configs = config.query_configs;
        self.adaptive_config = config.adaptive;
        self
    }

    /// Build the MQTT reaction
    pub fn build(self) -> anyhow::Result<MqttReaction> {
        let config = MqttReactionConfig {
            broker_addr: self.broker_addr,
            port: self.port,
            transport_mode: self.transport_mode,
            keep_alive: self.keep_alive,
            clean_session: self.clean_session,
            auth_mode: self.auth_mode,
            request_channel_capacity: self.request_channel_capacity,
            event_channel_capacity: self.event_channel_capacity,
            pending_throttle: self.pending_throttle,
            connection_timeout: self.connection_timeout,
            max_packet_size: self.max_packet_size,
            max_inflight: self.max_inflight,
            default_topic: self.default_topic,
            query_configs: self.query_configs,
            adaptive: self.adaptive_config,
        };

        Ok(MqttReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Reaction;

    #[test]
    fn test_mqtt_reaction_builder() {
        let reaction = MqttReactionBuilder::new("test-reaction")
            .with_broker_addr("mqtt.example.com")
            .with_port(1883)
            .with_queries(vec!["query1".to_string()])
            .with_default_topic("test/topic")
            .with_keep_alive(5000)
            .with_auth_mode(MqttAuthMode::UsernamePassword {
                username: "user".to_string(),
                password: "pass".to_string(),
            })
            .build()
            .expect("Failed to build MQTT reaction");

        assert_eq!(reaction.id(), "test-reaction");
        let props = reaction.properties();
        assert_eq!(
            props.get("auth_mode").unwrap(),
            &serde_json::Value::String(format!(
                "{:?}",
                MqttAuthMode::UsernamePassword {
                    username: "user".to_string(),
                    password: "pass".to_string(),
                }
            ))
        );
        assert_eq!(
            props.get("broker_addr").unwrap(),
            &serde_json::Value::String("mqtt.example.com".to_string())
        );
        assert_eq!(
            props.get("port").unwrap(),
            &serde_json::Value::Number(1883.into())
        );
        assert_eq!(
            props.get("keep_alive").unwrap(),
            &serde_json::Value::Number(5000.into())
        );
        assert_eq!(
            props.get("connection_timeout").unwrap(),
            &serde_json::Value::Number(default_connection_timeout().into())
        );
    }

    #[test]
    fn test_mqtt_reaction_builder_defaults() {
        let reaction = MqttReactionBuilder::new("default-reaction")
            .build()
            .unwrap();
        assert_eq!(reaction.id(), "default-reaction");
        let props = reaction.properties();

        assert_eq!(
            props.get("broker_addr").unwrap(),
            &serde_json::Value::String(default_broker_addr())
        );
        assert_eq!(
            props.get("port").unwrap(),
            &serde_json::Value::Number(default_port().into())
        );
        assert_eq!(
            props.get("transport_mode").unwrap(),
            &serde_json::Value::String(format!("{:?}", MqttTransportMode::default()))
        );
        assert_eq!(
            props.get("keep_alive").unwrap(),
            &serde_json::Value::Number(default_keep_alive().into())
        );
        assert_eq!(
            props.get("event_channel_capacity").unwrap(),
            &serde_json::Value::Number(default_event_channel_capacity().into()),
        );
        assert_eq!(
            props.get("connection_timeout").unwrap(),
            &serde_json::Value::Number(default_connection_timeout().into())
        );

        assert_eq!(
            props.get("auth_mode").unwrap(),
            &serde_json::Value::String(format!("{:?}", MqttAuthMode::default()))
        )
    }

    #[test]
    fn test_mqtt_reaction_properties_output() {
        let reaction = MqttReactionBuilder::new("props-reaction")
            .with_broker_addr("broker.internal")
            .with_port(1884)
            .with_transport_mode(MqttTransportMode::TLS {
                ca: vec![1, 2, 3],
                alpn: Some(vec![b"http/1.1".to_vec(), b"h2".to_vec()]),
                client_auth: Some((vec![4, 5, 6], vec![7, 8, 9])),
            })
            .with_keep_alive(42)
            .with_clean_session(false)
            .with_request_channel_capacity(16)
            .with_event_channel_capacity(32)
            .with_pending_throttle(11)
            .with_connection_timeout(7000)
            .with_max_packet_size(4096)
            .with_max_inflight(15)
            .with_auth_mode(MqttAuthMode::UsernamePassword {
                username: "u".to_string(),
                password: "p".to_string(),
            })
            .build()
            .expect("Failed to build MQTT reaction for properties test");

        let props = reaction.properties();

        assert_eq!(props.len(), 12);
        assert_eq!(
            props.get("broker_addr").unwrap(),
            &serde_json::Value::String("broker.internal".to_string())
        );
        assert_eq!(
            props.get("port").unwrap(),
            &serde_json::Value::Number(1884.into())
        );
        assert_eq!(
            props.get("transport_mode").unwrap(),
            &serde_json::Value::String("TLS".to_string())
        );
        assert_eq!(
            props.get("keep_alive").unwrap(),
            &serde_json::Value::Number(42.into())
        );
        assert_eq!(
            props.get("clean_session").unwrap(),
            &serde_json::Value::Bool(false)
        );
        assert_eq!(
            props.get("request_channel_capacity").unwrap(),
            &serde_json::Value::Number(16.into())
        );
        assert_eq!(
            props.get("event_channel_capacity").unwrap(),
            &serde_json::Value::Number(32.into())
        );
        assert_eq!(
            props.get("pending_throttle").unwrap(),
            &serde_json::Value::Number(11.into())
        );
        assert_eq!(
            props.get("connection_timeout").unwrap(),
            &serde_json::Value::Number(7000.into())
        );
        assert_eq!(
            props.get("max_packet_size").unwrap(),
            &serde_json::Value::Number(4096.into())
        );
        assert_eq!(
            props.get("max_inflight").unwrap(),
            &serde_json::Value::Number(15.into())
        );
        assert_eq!(
            props.get("auth_mode").unwrap(),
            &serde_json::Value::String(
                "UsernamePassword { username: \"u\", password: \"p\" }".to_string(),
            )
        );
    }

    #[test]
    fn test_mqtt_reaction_builder_with_config() {
        let config = MqttReactionConfig {
            broker_addr: "mqtt.example.com".to_string(),
            port: 1883,
            default_topic: "test/topic".to_string(),
            connection_timeout: 5000,
            query_configs: Default::default(),
            event_channel_capacity: 40,
            auth_mode: MqttAuthMode::UsernamePassword {
                username: "user".to_string(),
                password: "pass".to_string(),
            },
            clean_session: true,
            keep_alive: 60,
            max_inflight: 30,
            transport_mode: MqttTransportMode::TCP,
            pending_throttle: 100,
            max_packet_size: 2 * 1024,
            request_channel_capacity: 20,
            adaptive: None,
        };

        let reaction = MqttReactionBuilder::new("config-reaction")
            .with_config(config.clone())
            .with_queries(vec!["query1".to_string()])
            .build()
            .expect("Failed to build MQTT reaction with config");

        assert_eq!(reaction.id(), "config-reaction");
        let props = reaction.properties();
        assert_eq!(
            props.get("auth_mode").unwrap(),
            &serde_json::Value::String(format!("{:?}", config.auth_mode))
        );
        assert_eq!(
            props.get("broker_addr").unwrap(),
            &serde_json::Value::String(config.broker_addr)
        );
        assert_eq!(
            props.get("port").unwrap(),
            &serde_json::Value::Number(config.port.into())
        );
        assert_eq!(
            props.get("connection_timeout").unwrap(),
            &serde_json::Value::Number(config.connection_timeout.into())
        );
        assert_eq!(
            props.get("event_channel_capacity").unwrap(),
            &serde_json::Value::Number(config.event_channel_capacity.into())
        );

        assert_eq!(
            props.get("transport_mode").unwrap(),
            &serde_json::Value::String(format!("{:?}", config.transport_mode))
        );

        assert_eq!(
            props.get("max_packet_size").unwrap(),
            &serde_json::Value::Number(config.max_packet_size.into())
        );

        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_mqtt_reaction_builder_with_config_adaptive() {
        let config = MqttReactionConfig {
            broker_addr: "mqtt.example.com".to_string(),
            port: 1883,
            default_topic: "test/topic".to_string(),
            connection_timeout: 5000,
            query_configs: Default::default(),
            event_channel_capacity: 40,
            auth_mode: MqttAuthMode::UsernamePassword {
                username: "user".to_string(),
                password: "pass".to_string(),
            },
            clean_session: true,
            keep_alive: 60,
            max_inflight: 30,
            transport_mode: MqttTransportMode::TCP,
            pending_throttle: 100,
            max_packet_size: 2 * 1024,
            request_channel_capacity: 20,
            adaptive: Some(AdaptiveBatchConfig {
                adaptive_min_batch_size: 10,
                adaptive_max_batch_size: 100,
                adaptive_window_size: 5,
                adaptive_batch_timeout_ms: 2000,
            }),
        };

        let reaction = MqttReactionBuilder::new("config-reaction")
            .with_config(config.clone())
            .with_queries(vec!["query1".to_string()])
            .build()
            .expect("Failed to build MQTT reaction with config");

        assert_eq!(reaction.id(), "config-reaction");
        let props = reaction.properties();
        assert_eq!(
            props.get("auth_mode").unwrap(),
            &serde_json::Value::String(format!("{:?}", config.auth_mode))
        );
        assert_eq!(
            props.get("broker_addr").unwrap(),
            &serde_json::Value::String(config.broker_addr)
        );
        assert_eq!(
            props.get("port").unwrap(),
            &serde_json::Value::Number(config.port.into())
        );
        assert_eq!(
            props.get("connection_timeout").unwrap(),
            &serde_json::Value::Number(config.connection_timeout.into())
        );
        assert_eq!(
            props.get("event_channel_capacity").unwrap(),
            &serde_json::Value::Number(config.event_channel_capacity.into())
        );

        assert_eq!(
            props.get("transport_mode").unwrap(),
            &serde_json::Value::String(format!("{:?}", config.transport_mode))
        );

        assert_eq!(
            props.get("max_packet_size").unwrap(),
            &serde_json::Value::Number(config.max_packet_size.into())
        );

        assert_eq!(
            props.get("adaptive_batching").unwrap(),
            &serde_json::Value::String(format!("{:?}", config.adaptive))
        );

        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_mqtt_reaction_builder_with_query() {
        let reaction = MqttReactionBuilder::new("query-reaction")
            .with_query("query1")
            .with_query("query2")
            .build()
            .expect("Failed to build MQTT reaction with queries");

        assert_eq!(
            reaction.query_ids(),
            vec!["query1".to_string(), "query2".to_string()]
        );
    }
}
