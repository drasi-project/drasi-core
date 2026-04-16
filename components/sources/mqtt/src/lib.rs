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

mod adaptive_batcher;
pub mod config;
pub mod connection;
pub mod schema;

use adaptive_batcher::AdaptiveBatchConfig;
use config::{
    default_event_channel_capacity, default_host, default_port, default_qos, MqttConnectProperties,
    MqttSourceConfig, MqttSubscribeProperties, MqttTopicConfig, MqttTransportMode, TopicMapping,
};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};

use anyhow::Result;
use log::error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use drasi_lib::channels::{ComponentType, *};
use drasi_lib::{Source, SourceRuntimeContext};
mod mqtt;
mod pattern;
mod processor;
mod utils;

pub use config::MqttQoS;
pub use mqtt::MqttSource;
pub use schema::{MqttElement, MqttSourceChange};

/// Builder for MqttSource instances.
///
/// Provides a fluent API for constructing MQTT sources with sensible defaults
/// and adaptive batching settings. The builder takes the source ID at construction
/// and returns a fully constructed `MqttSource` from `build()`.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_mqtt::MqttSource;
///
/// let source = MqttSource::builder("my-source")
///     .with_host("0.0.0.0")
///     .with_port(8080)
///     .with_topic("my/mqtt/topic")
///     .with_qos(MqttQoS::ONE)
///     .with_adaptive_enabled(true)
///     .with_bootstrap_provider(my_provider)
///     .build()?;
/// ```
pub struct MqttSourceBuilder {
    id: String,
    host: String,
    port: u16,
    topics: Vec<MqttTopicConfig>,
    topic_mappings: Vec<TopicMapping>,
    event_channel_capacity: usize,
    transport: Option<MqttTransportMode>,
    request_channel_capacity: Option<usize>,
    max_inflight: Option<u16>,
    keep_alive: Option<u64>,
    clean_start: Option<bool>,
    max_incoming_packet_size: Option<usize>,
    max_outgoing_packet_size: Option<usize>,
    conn_timeout: Option<u64>,
    connect_properties: Option<MqttConnectProperties>,
    subscribe_properties: Option<MqttSubscribeProperties>,
    identity_provider: Option<Box<dyn drasi_lib::identity::IdentityProvider>>,
    adaptive_max_batch_size: Option<usize>,
    adaptive_min_batch_size: Option<usize>,
    adaptive_max_wait_ms: Option<u64>,
    adaptive_min_wait_ms: Option<u64>,
    adaptive_window_secs: Option<u64>,
    adaptive_enabled: Option<bool>,
    username: Option<String>,
    password: Option<String>,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl MqttSourceBuilder {
    /// Create a new MQTT source builder with the given source ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            host: default_host(),
            port: default_port(),
            identity_provider: None,
            topics: vec![],
            topic_mappings: vec![],
            event_channel_capacity: default_event_channel_capacity(),
            transport: None,
            request_channel_capacity: None,
            max_inflight: None,
            keep_alive: None,
            clean_start: None,

            max_incoming_packet_size: None, // v3
            max_outgoing_packet_size: None, // v3

            conn_timeout: None,         // v5
            connect_properties: None,   // v5
            subscribe_properties: None, // v5

            adaptive_max_batch_size: None,
            adaptive_min_batch_size: None,
            adaptive_max_wait_ms: None,
            adaptive_min_wait_ms: None,
            adaptive_window_secs: None,
            adaptive_enabled: None,

            username: None,
            password: None,

            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topics = vec![MqttTopicConfig {
            topic: topic.into(),
            qos: default_qos(),
        }];
        self
    }

    pub fn with_topic_config(mut self, topic: impl Into<String>, qos: MqttQoS) -> Self {
        self.topics.push(MqttTopicConfig {
            topic: topic.into(),
            qos,
        });
        self
    }

    pub fn with_topics(mut self, topics: Vec<MqttTopicConfig>) -> Self {
        self.topics = topics;
        self
    }

    pub fn with_timeout_ms(mut self, timeout: u64) -> Self {
        self.conn_timeout = Some(timeout);
        self
    }

    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity;
        self
    }

    pub fn with_transport(mut self, transport: MqttTransportMode) -> Self {
        self.transport = Some(transport);
        self
    }

    pub fn with_request_channel_capacity(mut self, capacity: usize) -> Self {
        self.request_channel_capacity = Some(capacity);
        self
    }

    pub fn with_max_inflight(mut self, max_inflight: u16) -> Self {
        self.max_inflight = Some(max_inflight);
        self
    }

    pub fn with_keep_alive_secs(mut self, keep_alive: u64) -> Self {
        self.keep_alive = Some(keep_alive);
        self
    }

    pub fn with_clean_start(mut self, clean_start: bool) -> Self {
        self.clean_start = Some(clean_start);
        self
    }

    pub fn with_max_incoming_packet_size(mut self, max_incoming_packet_size: usize) -> Self {
        self.max_incoming_packet_size = Some(max_incoming_packet_size);
        self
    }

    pub fn with_max_outgoing_packet_size(mut self, max_outgoing_packet_size: usize) -> Self {
        self.max_outgoing_packet_size = Some(max_outgoing_packet_size);
        self
    }

    pub fn with_connect_properties(mut self, connect_properties: MqttConnectProperties) -> Self {
        self.connect_properties = Some(connect_properties);
        self
    }

    pub fn with_subscribe_properties(
        mut self,
        subscribe_properties: MqttSubscribeProperties,
    ) -> Self {
        self.subscribe_properties = Some(subscribe_properties);
        self
    }

    pub fn with_topic_mappings(mut self, topic_mappings: Vec<TopicMapping>) -> Self {
        self.topic_mappings = topic_mappings;
        self
    }

    /// Set the identity provider for the source.
    pub fn with_identity_provider(
        mut self,
        identity_provider: impl drasi_lib::identity::IdentityProvider + 'static,
    ) -> Self {
        self.identity_provider = Some(Box::new(identity_provider));
        self
    }

    pub fn with_adaptive_max_batch_size(mut self, size: usize) -> Self {
        self.adaptive_max_batch_size = Some(size);
        self
    }

    pub fn with_adaptive_min_batch_size(mut self, size: usize) -> Self {
        self.adaptive_min_batch_size = Some(size);
        self
    }

    pub fn with_adaptive_max_wait_ms(mut self, wait_ms: u64) -> Self {
        self.adaptive_max_wait_ms = Some(wait_ms);
        self
    }

    pub fn with_adaptive_min_wait_ms(mut self, wait_ms: u64) -> Self {
        self.adaptive_min_wait_ms = Some(wait_ms);
        self
    }

    pub fn with_adaptive_window_secs(mut self, secs: u64) -> Self {
        self.adaptive_window_secs = Some(secs);
        self
    }

    pub fn with_adaptive_enabled(mut self, enabled: bool) -> Self {
        self.adaptive_enabled = Some(enabled);
        self
    }

    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    pub fn with_config(mut self, config: MqttSourceConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.identity_provider = config.identity_provider;
        self.topics = config.topics;
        self.topic_mappings = config.topic_mappings;
        self.event_channel_capacity = config.event_channel_capacity;
        self.transport = config.transport;
        self.request_channel_capacity = config.request_channel_capacity;
        self.max_inflight = config.max_inflight;
        self.keep_alive = config.keep_alive;
        self.clean_start = config.clean_start;
        self.max_incoming_packet_size = config.max_incoming_packet_size;
        self.max_outgoing_packet_size = config.max_outgoing_packet_size;
        self.conn_timeout = config.conn_timeout;
        self.connect_properties = config.connect_properties;
        self.subscribe_properties = config.subscribe_properties;
        self.adaptive_max_batch_size = config.adaptive_max_batch_size;
        self.adaptive_min_batch_size = config.adaptive_min_batch_size;
        self.adaptive_max_wait_ms = config.adaptive_max_wait_ms;
        self.adaptive_min_wait_ms = config.adaptive_min_wait_ms;
        self.adaptive_window_secs = config.adaptive_window_secs;
        self.adaptive_enabled = config.adaptive_enabled;
        self.username = config.username;
        self.password = config.password;
        self
    }

    pub async fn build(self) -> Result<MqttSource> {
        let config = MqttSourceConfig {
            host: self.host,
            port: self.port,
            identity_provider: self.identity_provider,
            topics: self.topics,
            topic_mappings: self.topic_mappings,
            event_channel_capacity: self.event_channel_capacity,
            transport: self.transport,
            request_channel_capacity: self.request_channel_capacity,
            max_inflight: self.max_inflight,
            keep_alive: self.keep_alive,
            clean_start: self.clean_start,
            max_incoming_packet_size: self.max_incoming_packet_size,
            max_outgoing_packet_size: self.max_outgoing_packet_size,
            conn_timeout: self.conn_timeout,
            connect_properties: self.connect_properties,
            subscribe_properties: self.subscribe_properties,
            adaptive_max_batch_size: self.adaptive_max_batch_size,
            adaptive_min_batch_size: self.adaptive_min_batch_size,
            adaptive_max_wait_ms: self.adaptive_max_wait_ms,
            adaptive_min_wait_ms: self.adaptive_min_wait_ms,
            adaptive_window_secs: self.adaptive_window_secs,
            adaptive_enabled: self.adaptive_enabled,
            username: self.username,
            password: self.password,
        };

        config.validate()?;

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        let mut adaptive_config = AdaptiveBatchConfig::default();

        if let Some(max_batch) = config.adaptive_max_batch_size {
            adaptive_config.max_batch_size = max_batch;
        }
        if let Some(min_batch) = config.adaptive_min_batch_size {
            adaptive_config.min_batch_size = min_batch;
        }
        if let Some(max_wait_ms) = config.adaptive_max_wait_ms {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config.adaptive_min_wait_ms {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config.adaptive_window_secs {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }
        if let Some(enabled) = config.adaptive_enabled {
            adaptive_config.adaptive_enabled = enabled;
        }

        MqttSource::from_parts(SourceBase::new(params)?, config, adaptive_config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod construction {
        use super::*;

        #[tokio::test]
        async fn test_builder_with_valid_config() {
            let source = MqttSourceBuilder::new("test-source")
                .with_host("localhost")
                .with_port(8080)
                .build()
                .await;
            assert!(source.is_ok());
        }

        #[tokio::test]
        async fn test_builder_with_custom_config() {
            let source = MqttSourceBuilder::new("mqtt-source")
                .with_host("0.0.0.0")
                .with_port(9000)
                .with_topic("my/mqtt/topic")
                .build()
                .await;
            assert!(source.is_ok());
            assert_eq!(source.unwrap().id(), "mqtt-source");
        }

        #[tokio::test]
        async fn test_with_dispatch_creates_source() {
            let config = MqttSourceConfig {
                host: "localhost".to_string(),
                port: 8080,
                identity_provider: None,
                topics: vec![MqttTopicConfig {
                    topic: "test/topic".to_string(),
                    qos: MqttQoS::TWO,
                }],
                topic_mappings: Vec::new(),
                event_channel_capacity: 100,
                transport: None,
                request_channel_capacity: None,
                max_inflight: None,
                keep_alive: None,
                clean_start: None,
                max_incoming_packet_size: None,
                max_outgoing_packet_size: None,
                conn_timeout: Some(10_000),
                connect_properties: None,
                subscribe_properties: None,
                adaptive_max_batch_size: None,
                adaptive_min_batch_size: None,
                adaptive_max_wait_ms: None,
                adaptive_min_wait_ms: None,
                adaptive_window_secs: None,
                adaptive_enabled: None,
                username: None,
                password: None,
            };
            let source = MqttSource::with_dispatch(
                "dispatch-source",
                config,
                Some(DispatchMode::Channel),
                Some(1000),
            )
            .await;
            assert!(source.is_ok());
            assert_eq!(source.unwrap().id(), "dispatch-source");
        }
    }

    mod properties {
        use super::*;

        #[tokio::test]
        async fn test_id_returns_correct_value() {
            let source = MqttSourceBuilder::new("my-mqtt-source")
                .with_host("localhost")
                .build()
                .await;
            assert!(source.is_ok());
            assert_eq!(source.unwrap().id(), "my-mqtt-source");
        }

        #[tokio::test]
        async fn test_type_name_returns_mqtt() {
            let source = MqttSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .await;
            assert!(source.is_ok());
            let unwrapped_source = source.unwrap();
            assert_eq!(unwrapped_source.type_name(), "mqtt");
        }

        #[tokio::test]
        async fn test_properties_contains_host_and_port() {
            let source = MqttSourceBuilder::new("test")
                .with_host("192.168.1.1")
                .with_port(9000)
                .build()
                .await;
            assert!(source.is_ok());
            let unwrapped_source = source.unwrap();
            let props = unwrapped_source.properties();

            assert_eq!(
                props.get("host"),
                Some(&serde_json::Value::String("192.168.1.1".to_string()))
            );
            assert_eq!(
                props.get("port"),
                Some(&serde_json::Value::Number(9000.into()))
            );
        }

        #[tokio::test]
        async fn test_properties_includes_topic_when_set() {
            let source = MqttSourceBuilder::new("test")
                .with_host("localhost")
                .with_topic("my/mqtt/topic")
                .build()
                .await;

            assert!(source.is_ok());
            let props = source.unwrap().properties();

            assert_eq!(
                props.get("topics"),
                Some(&serde_json::Value::Array(vec![serde_json::Value::String(
                    "my/mqtt/topic".to_string()
                )]))
            );
        }

        #[tokio::test]
        async fn test_properties_includes_topic_when_none() {
            let source = MqttSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .await;
            assert!(source.is_ok());
            let unwrapped_source = source.unwrap();
            let props = unwrapped_source.properties();

            assert!(props.contains_key("topics"));
        }
    }

    mod lifecycle {
        use super::*;

        #[tokio::test]
        async fn test_initial_status_is_stopped() {
            let source = MqttSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .await;
            assert!(source.is_ok());
            let unwrapped_source = source.unwrap();
            assert_eq!(unwrapped_source.status().await, ComponentStatus::Stopped);
        }
    }

    mod builder {
        use super::*;

        #[tokio::test]
        async fn test_mqtt_builder_defaults() {
            let source = MqttSourceBuilder::new("test").build().await;
            assert!(source.is_ok());
            let unwrapped_source = source.unwrap();
            assert_eq!(unwrapped_source.config().port, 1883);
            assert_eq!(unwrapped_source.config().topics.len(), 0);
        }

        #[tokio::test]
        async fn test_mqtt_builder_custom_values() {
            let source = MqttSourceBuilder::new("test")
                .with_host("api.example.com")
                .with_port(9000)
                .with_topic("/webhook")
                .with_timeout_ms(5000)
                .build()
                .await;

            assert!(source.is_ok());
            let unwrapped_source = source.unwrap();

            assert_eq!(unwrapped_source.config().host, "api.example.com");
            assert_eq!(unwrapped_source.config().port, 9000);
            assert_eq!(
                unwrapped_source.config().topics[0].topic,
                "/webhook".to_string()
            );
            assert_eq!(unwrapped_source.config().conn_timeout, Some(5000));
        }

        #[tokio::test]
        async fn test_mqtt_builder_adaptive_batching() {
            let source = MqttSourceBuilder::new("test")
                .with_host("localhost")
                .with_adaptive_max_batch_size(1000)
                .with_adaptive_min_batch_size(10)
                .with_adaptive_max_wait_ms(500)
                .with_adaptive_min_wait_ms(50)
                .with_adaptive_window_secs(60)
                .with_adaptive_enabled(true)
                .build()
                .await;

            assert_eq!(source.is_ok(), true);
            let unwrapped_source = source.unwrap();

            assert_eq!(
                unwrapped_source.config().adaptive_max_batch_size,
                Some(1000)
            );
            assert_eq!(unwrapped_source.config().adaptive_min_batch_size, Some(10));
            assert_eq!(unwrapped_source.config().adaptive_max_wait_ms, Some(500));
            assert_eq!(unwrapped_source.config().adaptive_min_wait_ms, Some(50));
            assert_eq!(unwrapped_source.config().adaptive_window_secs, Some(60));
            assert_eq!(unwrapped_source.config().adaptive_enabled, Some(true));
        }

        #[tokio::test]
        async fn test_builder_id() {
            let source = MqttSource::builder("my-mqtt-source")
                .with_host("localhost")
                .build()
                .await;
            assert!(source.is_ok());
            let unwrapped_source = source.unwrap();
            assert_eq!(unwrapped_source.id(), "my-mqtt-source");
        }
    }

    mod event_conversion {
        use crate::schema::convert_mqtt_to_source_change;
        use crate::schema::{MqttElement, MqttSourceChange};

        use super::*;

        #[test]
        fn test_convert_node_insert() {
            let mut props = serde_json::Map::new();
            props.insert(
                "name".to_string(),
                serde_json::Value::String("Alice".to_string()),
            );
            props.insert("age".to_string(), serde_json::Value::Number(30.into()));

            let mqtt_change = MqttSourceChange::Insert {
                element: MqttElement::Node {
                    id: "user-1".to_string(),
                    labels: vec!["User".to_string()],
                    properties: props,
                },
                timestamp: Some(12345678900),
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Insert { element } => match element {
                    drasi_core::models::Element::Node {
                        metadata,
                        properties,
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "user-1");
                        assert_eq!(metadata.labels.len(), 1);
                        assert_eq!(metadata.effective_from, 12345678900);
                        assert!(properties.get("name").is_some());
                        assert!(properties.get("age").is_some());
                    }
                    _ => panic!("Expected Node element"),
                },
                _ => panic!("Expected Insert operation"),
            }
        }

        #[test]
        fn test_convert_relation_insert() {
            let mqtt_change = MqttSourceChange::Insert {
                element: MqttElement::Relation {
                    id: "follows-1".to_string(),
                    labels: vec!["FOLLOWS".to_string()],
                    from: "user-1".to_string(),
                    to: "user-2".to_string(),
                    properties: serde_json::Map::new(),
                },
                timestamp: None,
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Insert { element } => match element {
                    drasi_core::models::Element::Relation {
                        metadata,
                        out_node,
                        in_node,
                        ..
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "follows-1");
                        assert_eq!(out_node.element_id.as_ref(), "user-1");
                        assert_eq!(in_node.element_id.as_ref(), "user-2");
                    }
                    _ => panic!("Expected Relation element"),
                },
                _ => panic!("Expected Insert operation"),
            }
        }

        #[test]
        fn test_convert_delete() {
            let mqtt_change = MqttSourceChange::Delete {
                id: "user-1".to_string(),
                labels: Some(vec!["User".to_string()]),
                timestamp: Some(9999999999),
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Delete { metadata } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "user-1");
                    assert_eq!(metadata.labels.len(), 1);
                }
                _ => panic!("Expected Delete operation"),
            }
        }

        #[test]
        fn test_convert_update() {
            let mqtt_change = MqttSourceChange::Update {
                element: MqttElement::Node {
                    id: "user-1".to_string(),
                    labels: vec!["User".to_string()],
                    properties: serde_json::Map::new(),
                },
                timestamp: None,
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Update { element } => match element {
                    drasi_core::models::Element::Node { metadata, .. } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "user-1");
                        assert_eq!(metadata.labels.len(), 1);
                    }
                    _ => panic!("Expected Node element"),
                },
                _ => panic!("Expected Update operation"),
            }
        }
    }

    mod adaptive_config {
        use super::*;

        #[tokio::test]
        async fn test_adaptive_config_from_mqtt_config() {
            let source = MqttSourceBuilder::new("test")
                .with_host("localhost")
                .with_topic("test/topic")
                .with_adaptive_max_batch_size(500)
                .with_adaptive_enabled(true)
                .build()
                .await;
            assert!(source.is_ok());

            // The adaptive config should be initialized from the mqtt config
            let unwrapped_source = source.unwrap();
            assert_eq!(unwrapped_source.adaptive_config().max_batch_size, 500);
            assert!(unwrapped_source.adaptive_config().adaptive_enabled);
        }

        #[tokio::test]
        async fn test_adaptive_config_uses_defaults_when_not_specified() {
            let source = MqttSourceBuilder::new("test")
                .with_host("localhost")
                .with_topic("test/topic")
                .build()
                .await;
            assert!(source.is_ok());

            // Should use AdaptiveBatchConfig defaults
            let default_config = AdaptiveBatchConfig::default();
            let unwrapped_source = source.unwrap();
            assert_eq!(
                unwrapped_source.adaptive_config().max_batch_size,
                default_config.max_batch_size
            );
            assert_eq!(
                unwrapped_source.adaptive_config().min_batch_size,
                default_config.min_batch_size
            );
        }
    }
}
