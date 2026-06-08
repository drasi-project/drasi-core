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

#![allow(unexpected_cfgs)]

//! Kafka Bootstrap Provider for Drasi
//!
//! This provider bootstraps initial data from a Kafka topic by consuming all
//! existing messages from the low watermark to the current high watermark.
//! Messages are processed through the mapping engine, which produces Insert,
//! Update, or Delete events depending on the mapping configuration. Tombstones
//! (null payload) are emitted as Delete events.
//! The final position (high watermark vector) is returned for seamless handoff
//! to streaming.

pub mod descriptor;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_source_kafka::encode_position;
use drasi_source_mapping::{SourceMapping, SourceMappingEngine};
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::message::Message;
use rdkafka::{ClientConfig, Offset, TopicPartitionList};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Kafka bootstrap provider that reads all existing messages from the topic.
///
/// Creates a temporary consumer that reads from the low watermark to the high
/// watermark for all partitions. Messages are processed through the mapping
/// engine, producing Insert, Update, or Delete events per the mapping config.
/// The final source_position is the high watermark vector, enabling seamless
/// handoff to the streaming consumer.
pub struct KafkaBootstrapProvider {
    config: KafkaBootstrapConfig,
    mappings: Vec<SourceMapping>,
    engine: Arc<SourceMappingEngine>,
}

/// Configuration for the Kafka bootstrap provider.
#[derive(Debug, Clone)]
pub struct KafkaBootstrapConfig {
    /// Kafka bootstrap servers (comma-separated)
    pub bootstrap_servers: String,
    /// Kafka topic to consume
    pub topic: String,
    /// Default node label for default mapping and tombstone handling
    pub node_label: String,
    /// Security protocol (PLAINTEXT, SASL_PLAINTEXT, SASL_SSL, SSL)
    pub security_protocol: Option<String>,
    /// SASL mechanism
    pub sasl_mechanism: Option<String>,
    /// SASL username
    pub sasl_username: Option<String>,
    /// SASL password
    pub sasl_password: Option<String>,
    /// Additional rdkafka configuration properties
    pub additional_properties: std::collections::HashMap<String, String>,
}

impl KafkaBootstrapProvider {
    /// Create a new Kafka bootstrap provider.
    pub fn new(config: KafkaBootstrapConfig, mappings: Vec<SourceMapping>) -> Self {
        Self {
            config,
            mappings,
            engine: Arc::new(SourceMappingEngine::new()),
        }
    }

    /// Create a builder for constructing a KafkaBootstrapProvider.
    pub fn builder() -> KafkaBootstrapProviderBuilder {
        KafkaBootstrapProviderBuilder::new()
    }

    /// Create a base consumer for bootstrap reading.
    fn create_consumer(&self, source_id: &str) -> Result<BaseConsumer> {
        let group_id = format!("__drasi_bootstrap_{}_{}", source_id, uuid::Uuid::new_v4());
        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", &self.config.bootstrap_servers)
            .set("group.id", &group_id)
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest");

        if let Some(ref protocol) = self.config.security_protocol {
            client_config.set("security.protocol", protocol);
        }
        if let Some(ref mechanism) = self.config.sasl_mechanism {
            client_config.set("sasl.mechanism", mechanism);
        }
        if let Some(ref username) = self.config.sasl_username {
            client_config.set("sasl.username", username);
        }
        if let Some(ref password) = self.config.sasl_password {
            client_config.set("sasl.password", password);
        }

        for (key, value) in &self.config.additional_properties {
            client_config.set(key, value);
        }

        let consumer: BaseConsumer = client_config
            .create()
            .context("Failed to create Kafka bootstrap consumer")?;

        Ok(consumer)
    }

    /// Process a message through the mapping engine.
    fn process_message(
        &self,
        payload: &JsonValue,
        key: &str,
        topic: &str,
        partition: usize,
        offset: i64,
        source_id: &str,
    ) -> Result<SourceChange> {
        let context = serde_json::json!({
            "payload": payload,
            "key": key,
            "topic": topic,
            "partition": partition,
            "offset": offset,
            "source_id": source_id,
        });

        let mapping = self
            .engine
            .find_matching_mapping(&self.mappings, &context, None)
            .ok_or_else(|| anyhow!("No matching mapping found for message"))?;

        self.engine.process_mapping(mapping, &context, source_id)
    }

    /// Process a tombstone (null payload) as a Delete event.
    fn process_tombstone(&self, key: &str, source_id: &str) -> Result<SourceChange> {
        if key.is_empty() {
            return Err(anyhow!(
                "Tombstone message has no key; cannot determine element to delete"
            ));
        }

        let metadata = drasi_core::models::ElementMetadata {
            reference: drasi_core::models::ElementReference {
                source_id: Arc::from(source_id),
                element_id: Arc::from(key),
            },
            labels: Arc::from(vec![Arc::from(self.config.node_label.as_str())]),
            effective_from: chrono::Utc::now().timestamp_millis() as u64,
        };

        Ok(SourceChange::Delete { metadata })
    }
}

#[async_trait]
impl BootstrapProvider for KafkaBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        info!(
            "Starting Kafka bootstrap for query '{}' from topic '{}'",
            request.query_id, self.config.topic
        );

        let consumer = self.create_consumer(&context.source_id)?;

        // Get topic metadata to discover partition count
        let metadata = consumer
            .fetch_metadata(Some(&self.config.topic), Duration::from_secs(10))
            .context("Failed to fetch topic metadata")?;

        let topic_metadata = metadata
            .topics()
            .iter()
            .find(|t| t.name() == self.config.topic)
            .ok_or_else(|| anyhow!("Topic '{}' not found", self.config.topic))?;

        let partition_count = topic_metadata.partitions().len();
        if partition_count == 0 {
            info!(
                "Topic '{}' has no partitions, nothing to bootstrap",
                self.config.topic
            );
            return Ok(BootstrapResult::default());
        }

        info!(
            "Kafka bootstrap: topic '{}' has {} partition(s)",
            self.config.topic, partition_count
        );

        // Fetch low/high watermarks for each partition.
        let mut low_watermarks: Vec<i64> = Vec::with_capacity(partition_count);
        let mut high_watermarks: Vec<i64> = Vec::with_capacity(partition_count);
        for partition in 0..partition_count as i32 {
            let (low, high) = consumer
                .fetch_watermarks(&self.config.topic, partition, Duration::from_secs(10))
                .context(format!(
                    "Failed to fetch watermarks for partition {partition}"
                ))?;
            if low < 0 || high < 0 || high < low {
                return Err(anyhow!(
                    "Invalid watermark range for partition {partition}: low={low}, high={high}"
                ));
            }
            low_watermarks.push(low);
            high_watermarks.push(high);
        }

        debug!(
            "Watermarks: low={:?}, high={:?}",
            low_watermarks, high_watermarks
        );

        // Check if topic is empty
        let total_messages: i64 = low_watermarks
            .iter()
            .zip(high_watermarks.iter())
            .map(|(low, high)| high - low)
            .sum();
        if total_messages == 0 {
            info!(
                "Topic '{}' is empty, nothing to bootstrap",
                self.config.topic
            );
            let position = encode_position(0, &high_watermarks);
            return Ok(BootstrapResult {
                event_count: 0,
                source_position: Some(position),
            });
        }

        // Assign consumer to all partitions starting from beginning
        let mut tpl = TopicPartitionList::new();
        for partition in 0..partition_count as i32 {
            tpl.add_partition_offset(&self.config.topic, partition, Offset::Beginning)
                .context("Failed to add partition offset")?;
        }
        consumer
            .assign(&tpl)
            .context("Failed to assign partitions")?;

        // Consume messages until we reach high watermarks for all partitions
        let mut current_offsets: Vec<i64> = low_watermarks.clone();
        let mut partition_done: Vec<bool> = low_watermarks
            .iter()
            .zip(high_watermarks.iter())
            .map(|(low, high)| high <= low)
            .collect();
        let mut event_count: usize = 0;

        let source_id = &context.source_id;
        let poll_timeout = Duration::from_millis(500);

        loop {
            // Check if all partitions are done
            if partition_done.iter().all(|&done| done) {
                break;
            }

            // Use block_in_place so the blocking poll doesn't starve the Tokio runtime.
            let poll_result = tokio::task::block_in_place(|| consumer.poll(poll_timeout));

            match poll_result {
                Some(Ok(msg)) => {
                    let partition = msg.partition() as usize;
                    let offset = msg.offset();
                    let key = msg
                        .key()
                        .map(|k| String::from_utf8_lossy(k).to_string())
                        .unwrap_or_default();
                    let topic = msg.topic().to_string();

                    // Update current offset
                    if partition < current_offsets.len() {
                        current_offsets[partition] = offset + 1;
                    }

                    // Check if we've reached the high watermark for this partition
                    if partition < partition_done.len()
                        && current_offsets[partition] >= high_watermarks[partition]
                    {
                        partition_done[partition] = true;
                    }

                    // Process the message
                    let maybe_change = if let Some(payload_bytes) = msg.payload() {
                        match serde_json::from_slice::<JsonValue>(payload_bytes) {
                            Ok(payload) => Some(self.process_message(
                                &payload, &key, &topic, partition, offset, source_id,
                            )),
                            Err(e) => {
                                warn!(
                                    "Kafka bootstrap: failed to parse JSON (partition={}, offset={}): {}",
                                    partition, offset, e
                                );
                                None
                            }
                        }
                    } else {
                        Some(self.process_tombstone(&key, source_id))
                    };

                    if let Some(change_result) = maybe_change {
                        match change_result {
                            Ok(change) => {
                                let sequence = context.next_sequence();
                                let bootstrap_event = BootstrapEvent {
                                    source_id: source_id.clone(),
                                    change,
                                    timestamp: chrono::Utc::now(),
                                    sequence,
                                };

                                event_tx
                                    .send(bootstrap_event)
                                    .await
                                    .map_err(|e| anyhow!("Failed to send bootstrap event: {e}"))?;

                                event_count += 1;
                            }
                            Err(e) => {
                                warn!(
                                    "Kafka bootstrap: failed to process message (partition={}, offset={}): {}",
                                    partition, offset, e
                                );
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    warn!("Kafka bootstrap: consumer error: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                None => {
                    // Poll returned no message (timeout) — continue checking
                }
            }
        }

        let position = encode_position(0, &high_watermarks);

        info!(
            "Kafka bootstrap completed for query '{}': {} events from {} partitions",
            request.query_id, event_count, partition_count
        );

        Ok(BootstrapResult {
            event_count,
            source_position: Some(position),
        })
    }
}

/// Builder for constructing a KafkaBootstrapProvider.
pub struct KafkaBootstrapProviderBuilder {
    bootstrap_servers: Option<String>,
    topic: Option<String>,
    node_label: Option<String>,
    mappings: Option<Vec<SourceMapping>>,
    security_protocol: Option<String>,
    sasl_mechanism: Option<String>,
    sasl_username: Option<String>,
    sasl_password: Option<String>,
    additional_properties: std::collections::HashMap<String, String>,
}

impl KafkaBootstrapProviderBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            bootstrap_servers: None,
            topic: None,
            node_label: None,
            mappings: None,
            security_protocol: None,
            sasl_mechanism: None,
            sasl_username: None,
            sasl_password: None,
            additional_properties: std::collections::HashMap::new(),
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

    pub fn additional_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.additional_properties.insert(key.into(), value.into());
        self
    }

    /// Build the KafkaBootstrapProvider.
    ///
    /// If no mappings are provided, a default mapping is created:
    /// key → ID, node_label → label, payload → properties, operation = insert.
    pub fn build(self) -> Result<KafkaBootstrapProvider> {
        let bootstrap_servers = self
            .bootstrap_servers
            .ok_or_else(|| anyhow!("bootstrap_servers is required"))?;
        let topic = self.topic.ok_or_else(|| anyhow!("topic is required"))?;
        let node_label = self
            .node_label
            .ok_or_else(|| anyhow!("node_label is required"))?;

        let config = KafkaBootstrapConfig {
            bootstrap_servers,
            topic,
            node_label: node_label.clone(),
            security_protocol: self.security_protocol,
            sasl_mechanism: self.sasl_mechanism,
            sasl_username: self.sasl_username,
            sasl_password: self.sasl_password,
            additional_properties: self.additional_properties,
        };

        let mappings = self
            .mappings
            .unwrap_or_else(|| vec![default_mapping(&node_label)]);

        Ok(KafkaBootstrapProvider::new(config, mappings))
    }
}

impl Default for KafkaBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the default mapping: key → ID, node_label → label, payload → properties, op = insert.
fn default_mapping(node_label: &str) -> SourceMapping {
    use drasi_source_mapping::{ElementTemplate, ElementType, OperationType};

    SourceMapping {
        operation: Some(OperationType::Insert),
        operation_from: None,
        operation_map: None,
        element_type: ElementType::Node,
        template: ElementTemplate {
            id: "{{key}}".to_string(),
            labels: vec![node_label.to_string()],
            properties: Some(serde_json::Value::String("{{payload}}".to_string())),
            from: None,
            to: None,
        },
        when: None,
        effective_from: None,
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "kafka-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::KafkaBootstrapDescriptor],
);

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_source_mapping::{ElementTemplate, ElementType, OperationType, SourceMapping};

    fn test_provider() -> KafkaBootstrapProvider {
        let config = KafkaBootstrapConfig {
            bootstrap_servers: "localhost:9092".to_string(),
            topic: "test-topic".to_string(),
            node_label: "TestNode".to_string(),
            security_protocol: None,
            sasl_mechanism: None,
            sasl_username: None,
            sasl_password: None,
            additional_properties: std::collections::HashMap::new(),
        };

        let mappings = vec![SourceMapping {
            when: None,
            element_type: ElementType::Node,
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            effective_from: None,
            template: ElementTemplate {
                id: "{{key}}".to_string(),
                labels: vec!["TestNode".to_string()],
                properties: None,
                from: None,
                to: None,
            },
        }];

        KafkaBootstrapProvider::new(config, mappings)
    }

    #[test]
    fn test_process_message_valid() {
        let provider = test_provider();
        let payload = serde_json::json!({"name": "Widget", "price": 42});
        let result = provider
            .process_message(&payload, "item-1", "test-topic", 0, 5, "src-1")
            .unwrap();

        match result {
            SourceChange::Insert { element } => {
                let meta = match &element {
                    drasi_core::models::Element::Node { metadata, .. } => metadata,
                    drasi_core::models::Element::Relation { metadata, .. } => metadata,
                };
                assert_eq!(&*meta.reference.element_id, "item-1");
                assert_eq!(&*meta.labels[0], "TestNode");
            }
            other => panic!("Expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn test_process_tombstone_valid_key() {
        let provider = test_provider();
        let result = provider.process_tombstone("order-99", "src-1").unwrap();

        match result {
            SourceChange::Delete { metadata } => {
                assert_eq!(&*metadata.reference.element_id, "order-99");
                assert_eq!(&*metadata.reference.source_id, "src-1");
                assert_eq!(&*metadata.labels[0], "TestNode");
            }
            other => panic!("Expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn test_process_tombstone_empty_key_returns_error() {
        let provider = test_provider();
        let result = provider.process_tombstone("", "src-1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no key"));
    }
}
