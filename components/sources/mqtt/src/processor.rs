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

use crate::schema::{convert_mqtt_to_source_change, MqttSourceChange};
use crate::utils::MqttPacket;
use crate::{config::MqttSourceConfig, pattern::PatternMatcher};
use drasi_batching_common::{AdaptiveBatchConfig, AdaptiveBatcher};
use drasi_lib::channels::SourceChangeEvent;
use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
use drasi_lib::SourceBase;
use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct MqttProcessor {
    mapper: Arc<PatternMatcher>,
}

impl MqttProcessor {
    //...... public methods

    pub fn new(config: &MqttSourceConfig) -> Self {
        Self {
            mapper: Arc::new(PatternMatcher::new(&config.topic_mappings)),
        }
    }

    pub fn start_processing_loop(
        &mut self,
        source_id: String,
        mut rx: mpsc::Receiver<MqttPacket>,
        batch_tx: mpsc::Sender<SourceChangeEvent>,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<JoinHandle<()>> {
        let pattern_matcher = Arc::clone(&self.mapper);
        Ok(tokio::spawn(async move {
            Self::run_processing_loop(
                source_id.to_string(),
                pattern_matcher,
                rx,
                batch_tx,
                cancellation_token,
            )
            .await;
        }))
    }

    pub fn start_adaptive_batcher_loop(
        &mut self,
        source_id: String,
        batch_rx: mpsc::Receiver<SourceChangeEvent>,
        dispatchers: Arc<
            tokio::sync::RwLock<
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
        adaptive_config: AdaptiveBatchConfig,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<JoinHandle<()>> {
        Ok(tokio::spawn(async move {
            Self::run_adaptive_batcher_loop(
                batch_rx,
                dispatchers,
                adaptive_config,
                cancellation_token,
                source_id,
            )
            .await;
        }))
    }

    //...... internal processing methods

    async fn run_adaptive_batcher_loop(
        batch_rx: mpsc::Receiver<SourceChangeEvent>,
        dispatchers: Arc<
            tokio::sync::RwLock<
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
        adaptive_config: AdaptiveBatchConfig,
        cancellation_token: CancellationToken,
        source_id: String,
    ) {
        let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config.clone());
        let mut total_events = 0u64;
        let mut total_batches = 0u64;

        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    info!("[{source_id}] MQTT  batching loop stopped - cancellation requested");
                    break;
                }

                batch = batcher.next_batch() => {

                    if let Some(batch) = batch {

                        if batch.is_empty() {
                            debug!("[{source_id}] MQTT  batcher received empty batch, skipping");
                            continue;
                        }

                        let batch_size = batch.len();

                        total_events += batch_size as u64;
                        total_batches += 1;

                        debug!(
                            "[{source_id}] MQTT  batcher forwarding batch #{total_batches} with {batch_size} events to dispatchers"
                        );

                        let mut sent_count = 0;
                        let mut failed_count = 0;

                        for (idx, event) in batch.into_iter().enumerate() {
                            debug!(
                                "[{source_id}] Batch #{total_batches}, dispatching event {}/{batch_size}",
                                idx + 1
                            );

                            let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                            profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                            let wrapper = SourceEventWrapper::with_profiling(
                                event.source_id.clone(),
                                SourceEvent::Change(event.change),
                                event.timestamp,
                                profiling,
                            );

                            if let Err(e) =
                                SourceBase::dispatch_from_task(dispatchers.clone(), wrapper.clone(), &source_id)
                                    .await
                            {
                                error!(
                                    "[{source_id}] Batch #{total_batches}, failed to dispatch event {}/{batch_size} (no subscribers): {e}", idx + 1
                                );
                                failed_count += 1;
                            } else {
                                debug!(
                                    "[{source_id}] Batch #{total_batches}, successfully dispatched event {}/{batch_size}", idx + 1
                                );
                                sent_count += 1;
                            }
                        }

                        debug!(
                            "[{source_id}] Batch #{total_batches} complete: {sent_count} dispatched, {failed_count} failed"
                        );

                        if total_batches.is_multiple_of(100) {
                            info!(
                                "[{}] Adaptive MQTT metrics - Batches: {}, Events: {}, Avg batch size: {:.1}",
                                source_id,
                                total_batches,
                                total_events,
                                total_events as f64 / total_batches as f64
                            );
                        }
                    } else {
                        info!(
                            "[{source_id}] MQTT batching loop stopped - Total batches: {total_batches}, Total events: {total_events}"
                        );
                        break;
                    }
                }
            }
        }
    }

    async fn run_processing_loop(
        source_id: String,
        matcher: Arc<PatternMatcher>,
        mut rx: mpsc::Receiver<MqttPacket>,
        batch_tx: mpsc::Sender<SourceChangeEvent>,
        cancellation_token: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    info!("[{source_id}] MQTT processing loop stopped - cancellation requested");
                    break;
                }
                packet = rx.recv() => {
                    if let Some(packet) = packet {
                        // generate source changes from the packet and topic name
                        let source_changes = Self::process(&matcher, &packet);

                        // send to the batcher
                        Self::send_to_batcher(
                            &source_id,
                            &batch_tx,
                            source_changes,
                            &cancellation_token,
                        )
                        .await;
                    } else {
                        info!("[{source_id}] MQTT processing loop stopped - channel closed");
                        break;
                    }
                }
            }
        }
    }

    fn process(mapper: &PatternMatcher, packet: &MqttPacket) -> Vec<MqttSourceChange> {
        match mapper.generate_schema(packet) {
            Ok(changes) => changes,
            Err(e) => {
                warn!(
                    "Error processing MQTT packet with topic '{}': {e}",
                    packet.topic
                );
                vec![]
            }
        }
    }

    async fn send_to_batcher(
        source_id: &str,
        batch_tx: &mpsc::Sender<SourceChangeEvent>,
        source_changes: Vec<MqttSourceChange>,
        cancellation_token: &CancellationToken,
    ) {
        let source_id = source_id.to_string();
        for (idx, change) in source_changes.into_iter().enumerate() {
            if cancellation_token.is_cancelled() {
                debug!("[{source_id}] Stopping send-to-batcher loop due to cancellation");
                break;
            }

            let timestamp = match &change {
                MqttSourceChange::Update { timestamp, .. } => timestamp.unwrap_or_else(|| {
                    let now = std::time::SystemTime::now();
                    now.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_else(|e| {
                            error!("System time is before UNIX EPOCH: {e}");
                            std::time::Duration::from_millis(0)
                        })
                        .as_millis() as u64
                }),
            };
            match convert_mqtt_to_source_change(&change, &source_id) {
                Ok(source_change) => {
                    let change_event = SourceChangeEvent {
                        source_id: source_id.to_string(),
                        change: source_change,
                        timestamp: chrono::DateTime::<chrono::Utc>::from(
                            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp),
                        ),
                        sequence: None,
                    };

                    tokio::select! {
                        _ = cancellation_token.cancelled() => {
                            debug!("[{source_id}] Cancelled while waiting to send change {idx} to batcher");
                            break;
                        }
                        send_result = batch_tx.send(change_event) => {
                            if let Err(e) = send_result {
                                error!(
                                    "[{source_id}] Failed to send change event to batcher for change {idx}: {e}"
                                );
                            } else {
                                debug!(
                                    "[{source_id}] Successfully sent change event to batcher for change {idx}",
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "[{source_id}] Failed to convert MQTT change to source change for change {idx}: {e}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        MappingEntity, MappingMode, MappingNode, MappingProperties, MappingRelation,
        MappingRelationEndpoint, TopicMapping,
    };
    use crate::MqttElement;
    use bytes::Bytes;

    fn simple_mapping() -> TopicMapping {
        TopicMapping {
            pattern: "sensors/{room}/{device}".to_string(),
            entity: MappingEntity {
                label: "Sensor".to_string(),
                id: "{room}:{device}".to_string(),
            },
            properties: MappingProperties {
                mode: MappingMode::PayloadAsField,
                field_name: Some("value".to_string()),
                inject_id: Some(false),
                inject: vec![],
            },
            nodes: vec![MappingNode {
                label: "Room".to_string(),
                id: "{room}".to_string(),
            }],
            relations: vec![MappingRelation {
                label: "IN_ROOM".to_string(),
                id: "{room}:{device}_in_{room}".to_string(),
                from: MappingRelationEndpoint {
                    label: "Sensor".to_string(),
                    id: "{room}:{device}".to_string(),
                },
                to: MappingRelationEndpoint {
                    label: "Room".to_string(),
                    id: "{room}".to_string(),
                },
            }],
        }
    }

    fn packet(topic: &str, payload: &[u8], timestamp: u64) -> MqttPacket {
        MqttPacket {
            topic: topic.to_string(),
            payload: Bytes::from(payload.to_vec()),
            timestamp,
        }
    }

    #[test]
    fn process_matching_topic_returns_source_changes() {
        let matcher = PatternMatcher::new(&vec![simple_mapping()]);
        let pkt = packet("sensors/lab1/thermometer", b"22", 9000);

        let changes = MqttProcessor::process(&matcher, &pkt);

        assert_eq!(changes.len(), 3, "expected 3 changes for a matching packet");

        match &changes[0] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(9000));
                match element {
                    MqttElement::Node {
                        id,
                        labels,
                        properties,
                    } => {
                        assert_eq!(id, "lab1:thermometer");
                        assert_eq!(labels, &vec!["Sensor".to_string()]);
                        assert_eq!(
                            properties.get("value"),
                            Some(&serde_json::Value::Number(22.into()))
                        );
                    }
                    _ => panic!("first change should be a node"),
                }
            }
        }

        match &changes[1] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(9000));
                match element {
                    MqttElement::Node { id, labels, .. } => {
                        assert_eq!(id, "lab1");
                        assert_eq!(labels, &vec!["Room".to_string()]);
                    }
                    _ => panic!("second change should be a node"),
                }
            }
        }

        match &changes[2] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(9000));
                match element {
                    MqttElement::Relation {
                        id,
                        labels,
                        from,
                        to,
                        ..
                    } => {
                        assert_eq!(id, "lab1:thermometer_in_lab1");
                        assert_eq!(labels, &vec!["IN_ROOM".to_string()]);
                        assert_eq!(from, "lab1:thermometer");
                        assert_eq!(to, "lab1");
                    }
                    _ => panic!("third change should be a relation"),
                }
            }
        }
    }

    #[test]
    fn process_unmatched_topic_returns_empty_vec() {
        let matcher = PatternMatcher::new(&vec![simple_mapping()]);
        let pkt = packet("telemetry/unknown/device", b"42", 1000);

        let changes = MqttProcessor::process(&matcher, &pkt);

        assert!(
            changes.is_empty(),
            "expected empty vec for an unmatched topic, got {changes:?}"
        );
    }
}
