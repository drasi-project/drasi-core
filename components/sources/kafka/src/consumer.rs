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

//! Kafka consumer task that reads messages and dispatches them as graph change events.

use crate::config::KafkaSourceConfig;
use crate::position::{decode_partition_offsets, encode_position};
use anyhow::{anyhow, Context, Result};
use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
use drasi_lib::channels::events::{SourceEvent, SourceEventWrapper};
use drasi_lib::sources::SourceBase;
use drasi_source_mapping::{SourceMapping, SourceMappingEngine};
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;
use rdkafka::{ClientConfig, Offset, TopicPartitionList};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
use tracing::{error, info, warn};

/// Consumer task that runs in a spawned Tokio task.
pub struct KafkaConsumerTask {
    pub config: KafkaSourceConfig,
    pub mappings: Vec<SourceMapping>,
    pub engine: Arc<SourceMappingEngine>,
    pub base: SourceBase,
    pub source_id: String,
    pub shutdown_rx: watch::Receiver<bool>,
    pub resume_positions: Arc<RwLock<HashMap<String, Vec<i64>>>>,
    pub await_bootstrap_boundary: Arc<AtomicBool>,
}

impl KafkaConsumerTask {
    /// Run the consumer task. Returns when shutdown is signaled or an error occurs.
    pub async fn run(mut self) -> Result<()> {
        let consumer = self.create_consumer()?;

        // Don't consume until at least one query has subscribed, otherwise events
        // can be dropped before any dispatcher exists. Also watch for shutdown so
        // stopping before any subscription doesn't hang forever.
        tokio::select! {
            _ = self.base.wait_for_subscribers() => {}
            _ = self.shutdown_rx.changed() => {
                if *self.shutdown_rx.borrow() {
                    info!("[{}] Shutdown before any subscribers", self.source_id);
                    return Ok(());
                }
            }
        }

        // Get topic metadata to discover partition count
        let metadata = consumer
            .fetch_metadata(Some(&self.config.topic), std::time::Duration::from_secs(10))
            .context("Failed to fetch topic metadata")?;

        let topic_metadata = metadata
            .topics()
            .iter()
            .find(|t| t.name() == self.config.topic)
            .ok_or_else(|| anyhow!("Topic '{}' not found", self.config.topic))?;

        let partition_count = topic_metadata.partitions().len();
        if partition_count == 0 {
            return Err(anyhow!("Topic '{}' has no partitions", self.config.topic));
        }

        info!(
            "[{}] Topic '{}' has {} partition(s)",
            self.source_id, self.config.topic, partition_count
        );

        let min_resume_offsets = self.minimum_resume_offsets(partition_count).await;
        let bootstrap_offsets = if min_resume_offsets.is_none()
            && self.await_bootstrap_boundary.load(Ordering::Acquire)
        {
            match self
                .wait_for_initial_bootstrap_offsets(partition_count)
                .await
            {
                Ok(offsets) => Some(offsets),
                // A shutdown during the boundary wait is a clean exit, not an error.
                Err(e) if *self.shutdown_rx.borrow() => {
                    info!("[{}] Kafka consumer shutting down: {e}", self.source_id);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };

        // Assign to all partitions.
        let mut tpl = TopicPartitionList::new();
        for partition in 0..partition_count as i32 {
            let start_offset = if let Some(ref offsets) = min_resume_offsets {
                Offset::Offset(offsets.get(partition as usize).copied().unwrap_or(0).max(0))
            } else if let Some(ref offsets) = bootstrap_offsets {
                Offset::Offset(offsets[partition as usize].max(0))
            } else {
                match self.config.auto_offset_reset {
                    crate::config::AutoOffsetReset::Earliest => Offset::Beginning,
                    crate::config::AutoOffsetReset::Latest => Offset::End,
                }
            };

            tpl.add_partition_offset(&self.config.topic, partition, start_offset)
                .context("Failed to add partition offset")?;
        }

        consumer
            .assign(&tpl)
            .context("Failed to assign partitions")?;

        info!(
            "[{}] Assigned to {} partitions of '{}'",
            self.source_id, partition_count, self.config.topic
        );
        if let Some(ref offsets) = min_resume_offsets {
            info!(
                "[{}] Starting from replay offsets: {:?}",
                self.source_id, offsets
            );
        } else if let Some(ref offsets) = bootstrap_offsets {
            info!(
                "[{}] Starting from bootstrap boundary offsets: {:?}",
                self.source_id, offsets
            );
        }

        // Track current offsets per partition (next offset to consume).
        // For Latest mode without resume, use high watermarks so checkpoints
        // don't incorrectly encode offset 0.
        let mut current_offsets = if let Some(offsets) = min_resume_offsets {
            offsets
        } else if let Some(offsets) = bootstrap_offsets {
            offsets
        } else if matches!(
            self.config.auto_offset_reset,
            crate::config::AutoOffsetReset::Latest
        ) {
            let mut watermarks = Vec::with_capacity(partition_count);
            for p in 0..partition_count as i32 {
                let (_, high) = consumer
                    .fetch_watermarks(&self.config.topic, p, std::time::Duration::from_secs(5))
                    .unwrap_or((0, 0));
                watermarks.push(high);
            }
            watermarks
        } else {
            vec![0; partition_count]
        };

        // Consume messages
        loop {
            tokio::select! {
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        info!("[{}] Shutdown signal received", self.source_id);
                        break;
                    }
                }
                message_result = consumer.recv() => {
                    match message_result {
                        Ok(msg) => {
                            let partition = msg.partition() as usize;
                            let offset = msg.offset();
                            let key = msg.key()
                                .map(|k| String::from_utf8_lossy(k).to_string())
                                .unwrap_or_default();
                            let topic = msg.topic().to_string();

                            let source_change = if let Some(payload_bytes) = msg.payload() {
                                match serde_json::from_slice::<JsonValue>(payload_bytes) {
                                    Ok(payload) => self.process_message(&payload, &key, &topic, partition, offset),
                                    Err(e) => {
                                        warn!(
                                            "[{}] Failed to parse message payload as JSON (topic={}, partition={}, offset={}): {}",
                                            self.source_id, topic, partition, offset, e
                                        );
                                        continue;
                                    }
                                }
                            } else {
                                self.process_tombstone(&key)
                            };

                            match source_change {
                                Ok(change) => {
                                    let mut offsets_for_event = current_offsets.clone();
                                    if partition < offsets_for_event.len() {
                                        offsets_for_event[partition] = offset + 1;
                                    }

                                    let wrapper = SourceEventWrapper {
                                        source_id: self.source_id.clone(),
                                        event: SourceEvent::Change(change),
                                        timestamp: chrono::Utc::now(),
                                        profiling: None,
                                        sequence: None,
                                        source_position: Some(encode_position(partition, &offsets_for_event)),
                                    };

                                    if let Err(e) = self.base.dispatch_event(wrapper).await {
                                        error!("[{}] Failed to dispatch event: {}", self.source_id, e);
                                    } else if partition < current_offsets.len() {
                                        // Advance only after successful dispatch so failed events
                                        // are replayable after restart.
                                        current_offsets[partition] = offset + 1;
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "[{}] Failed to process message (topic={}, partition={}, offset={}): {}",
                                        self.source_id, topic, partition, offset, e
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!("[{}] Kafka consumer error: {}", self.source_id, e);
                            // rdkafka may return transient errors; continue unless shutdown
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn wait_for_initial_bootstrap_offsets(
        &mut self,
        partition_count: usize,
    ) -> Result<Vec<i64>> {
        info!(
            "[{}] Waiting for Kafka bootstrap boundary before initial consumption",
            self.source_id
        );
        let boundary = tokio::select! {
            boundary = self.base.wait_for_bootstrap_boundary() => boundary,
            _ = self.shutdown_rx.changed() => {
                if *self.shutdown_rx.borrow() {
                    return Err(anyhow!("shutting down while waiting for bootstrap boundary"));
                }
                self.base.wait_for_bootstrap_boundary().await
            }
        };
        let boundary = boundary.ok_or_else(|| anyhow!("Bootstrap boundary channel closed"))?;
        let partition_offsets = decode_partition_offsets(&boundary)
            .ok_or_else(|| anyhow!("Invalid Kafka bootstrap boundary position"))?;
        Self::partition_offsets_to_vec(partition_offsets, partition_count)
    }

    fn partition_offsets_to_vec(
        partition_offsets: Vec<(i32, i64)>,
        partition_count: usize,
    ) -> Result<Vec<i64>> {
        let mut offsets: Vec<Option<i64>> = vec![None; partition_count];
        for (partition, offset) in partition_offsets {
            if partition < 0 || partition as usize >= partition_count {
                return Err(anyhow!(
                    "Bootstrap boundary contains partition {partition}, expected 0..{}",
                    partition_count.saturating_sub(1)
                ));
            }
            if offsets[partition as usize].replace(offset).is_some() {
                return Err(anyhow!(
                    "Bootstrap boundary contains duplicate partition {partition}"
                ));
            }
        }
        offsets
            .into_iter()
            .enumerate()
            .map(|(partition, offset)| {
                offset.ok_or_else(|| anyhow!("Bootstrap boundary missing partition {partition}"))
            })
            .collect()
    }

    async fn minimum_resume_offsets(&self, partition_count: usize) -> Option<Vec<i64>> {
        let resume_positions = self.resume_positions.read().await;
        let mut min_offsets: Option<Vec<i64>> = None;

        for (query_id, offsets) in resume_positions.iter() {
            if offsets.len() != partition_count {
                warn!(
                    "[{}] Ignoring resume offsets for query '{}' due to partition count mismatch (got {}, expected {})",
                    self.source_id,
                    query_id,
                    offsets.len(),
                    partition_count
                );
                continue;
            }

            if let Some(ref mut min) = min_offsets {
                for i in 0..partition_count {
                    min[i] = min[i].min(offsets[i]);
                }
            } else {
                min_offsets = Some(offsets.clone());
            }
        }

        min_offsets
    }

    /// Create the rdkafka StreamConsumer with configuration.
    fn create_consumer(&self) -> Result<StreamConsumer> {
        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", &self.config.bootstrap_servers)
            .set("group.id", &self.config.group_id)
            .set("enable.auto.commit", "false")
            .set(
                "auto.offset.reset",
                match self.config.auto_offset_reset {
                    crate::config::AutoOffsetReset::Earliest => "earliest",
                    crate::config::AutoOffsetReset::Latest => "latest",
                },
            );

        // Additional properties applied FIRST so explicit settings below always win.
        for (key, value) in &self.config.additional_properties {
            client_config.set(key, value);
        }

        // Security configuration (applied after additional_properties to prevent override)
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

        let consumer: StreamConsumer = client_config
            .create()
            .context("Failed to create Kafka consumer")?;

        Ok(consumer)
    }

    /// Process a non-tombstone message through the mapping engine.
    fn process_message(
        &self,
        payload: &JsonValue,
        key: &str,
        topic: &str,
        partition: usize,
        offset: i64,
    ) -> Result<SourceChange> {
        // Build template context
        let context = serde_json::json!({
            "payload": payload,
            "key": key,
            "topic": topic,
            "partition": partition,
            "offset": offset,
            "source_id": self.source_id,
        });

        // Find matching mapping
        let mapping = self
            .engine
            .find_matching_mapping(&self.mappings, &context, None)
            .ok_or_else(|| anyhow!("No matching mapping found for message"))?;

        self.engine
            .process_mapping(mapping, &context, &self.source_id)
    }

    /// Process a tombstone (null payload) as a Delete event.
    fn process_tombstone(&self, key: &str) -> Result<SourceChange> {
        if key.is_empty() {
            return Err(anyhow!(
                "Tombstone message has no key; cannot determine element to delete"
            ));
        }

        let metadata = ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(self.source_id.as_str()),
                element_id: Arc::from(key),
            },
            labels: Arc::from(vec![Arc::from(self.config.node_label.as_str())]),
            effective_from: chrono::Utc::now().timestamp_millis() as u64,
        };

        Ok(SourceChange::Delete { metadata })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutoOffsetReset;
    use drasi_lib::sources::SourceBaseParams;
    use drasi_lib::DispatchMode;
    use drasi_source_mapping::{ElementTemplate, ElementType, OperationType};

    /// Helper to create a minimal KafkaConsumerTask for testing.
    fn test_task(mappings: Vec<SourceMapping>) -> KafkaConsumerTask {
        let config = KafkaSourceConfig {
            id: "test-source".to_string(),
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

        let params = SourceBaseParams::new("test-source").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).expect("SourceBase creation failed");
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        KafkaConsumerTask {
            config,
            mappings,
            engine: Arc::new(SourceMappingEngine::new()),
            base,
            source_id: "test-source".to_string(),
            shutdown_rx,
            resume_positions: Arc::new(RwLock::new(HashMap::new())),
            await_bootstrap_boundary: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Default mapping: key → id, payload → properties
    fn default_mapping() -> Vec<SourceMapping> {
        vec![SourceMapping {
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
        }]
    }

    #[test]
    fn test_process_tombstone_empty_key_returns_error() {
        let task = test_task(vec![]);
        let result = task.process_tombstone("");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no key"));
    }

    #[test]
    fn test_process_tombstone_valid_key() {
        let task = test_task(vec![]);
        let result = task.process_tombstone("order-42").unwrap();

        match result {
            SourceChange::Delete { metadata } => {
                assert_eq!(&*metadata.reference.element_id, "order-42");
                assert_eq!(&*metadata.reference.source_id, "test-source");
                assert_eq!(&*metadata.labels[0], "TestNode");
            }
            _ => panic!("Expected SourceChange::Delete, got {result:?}"),
        }
    }

    #[test]
    fn test_process_message_no_matching_mapping() {
        // Empty mappings list — no mapping can match
        let task = test_task(vec![]);
        let payload = serde_json::json!({"id": "123"});
        let result = task.process_message(&payload, "key-1", "topic", 0, 0);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No matching mapping"));
    }

    #[test]
    fn test_process_message_with_valid_mapping() {
        let task = test_task(default_mapping());
        let payload = serde_json::json!({"name": "Widget", "price": 42});
        let result = task.process_message(&payload, "item-1", "orders", 0, 5);
        assert!(result.is_ok());

        match result.unwrap() {
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

    #[tokio::test]
    async fn test_minimum_resume_offsets_no_queries() {
        let task = test_task(vec![]);
        let result = task.minimum_resume_offsets(3).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_minimum_resume_offsets_element_wise_minimum() {
        let task = test_task(vec![]);
        {
            let mut positions = task.resume_positions.write().await;
            positions.insert("query-a".to_string(), vec![10, 5, 8]);
            positions.insert("query-b".to_string(), vec![7, 9, 3]);
        }

        let result = task.minimum_resume_offsets(3).await;
        assert_eq!(result, Some(vec![7, 5, 3]));
    }

    #[tokio::test]
    async fn test_minimum_resume_offsets_partition_count_mismatch_skipped() {
        let task = test_task(vec![]);
        {
            let mut positions = task.resume_positions.write().await;
            // This one has wrong length (2 instead of 3) — should be skipped
            positions.insert("query-bad".to_string(), vec![1, 2]);
            positions.insert("query-good".to_string(), vec![5, 5, 5]);
        }

        let result = task.minimum_resume_offsets(3).await;
        // Only query-good matches, so result is its offsets
        assert_eq!(result, Some(vec![5, 5, 5]));
    }
}
