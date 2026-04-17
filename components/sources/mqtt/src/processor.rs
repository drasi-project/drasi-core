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

use crate::adaptive_batcher::{AdaptiveBatchConfig, AdaptiveBatcher};
use crate::schema::{convert_mqtt_to_source_change, MqttSourceChange};
use crate::utils::MqttPacket;
use crate::{config::MqttSourceConfig, pattern::PatternMatcher};
use drasi_lib::channels::SourceChangeEvent;
use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
use drasi_lib::SourceBase;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct MqttProcessor {
    mapper: Arc<PatternMatcher>,
    processing_loop_handle: Option<JoinHandle<()>>,
    adaptive_batcher_loop_handle: Option<JoinHandle<()>>,
    adaptive: bool,
}

impl MqttProcessor {
    //...... public methods

    pub fn new(config: &MqttSourceConfig) -> Self {
        Self {
            mapper: Arc::new(PatternMatcher::new(&config.topic_mappings)),
            processing_loop_handle: None,
            adaptive_batcher_loop_handle: None,
            adaptive: config.adaptive_enabled.unwrap_or(false),
        }
    }

    pub fn start_processing_loop(
        &mut self,
        source_id: String,
        mut rx: mpsc::Receiver<MqttPacket>,
        batch_tx: mpsc::Sender<SourceChangeEvent>,
    ) {
        let pattern_matcher = Arc::clone(&self.mapper);
        self.processing_loop_handle = Some(tokio::spawn(async move {
            Self::run_processing_loop(source_id.to_string(), pattern_matcher, rx, batch_tx).await;
        }));
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
    ) {
        self.adaptive_batcher_loop_handle = Some(tokio::spawn(async move {
            Self::run_adaptive_batcher_loop(batch_rx, dispatchers, adaptive_config, source_id)
                .await;
        }));
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
        source_id: String,
    ) {
        let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config.clone());
        let mut total_events = 0u64;
        let mut total_batches = 0u64;

        while let Some(batch) = batcher.next_batch().await {
            if batch.is_empty() {
                debug!("[{source_id}] MQTT Batcher received empty batch, skipping");
                continue;
            }

            let batch_size = batch.len();

            total_events += batch_size as u64;
            total_batches += 1;

            debug!(
                "[{source_id}] MQTT Batcher forwarding batch #{total_batches} with {batch_size} events to dispatchers"
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
        }

        info!(
            "[{source_id}] Adaptive MQTT batcher stopped - Total batches: {total_batches}, Total events: {total_events}"
        );
    }

    async fn run_processing_loop(
        source_id: String,
        matcher: Arc<PatternMatcher>,
        mut rx: mpsc::Receiver<MqttPacket>,
        batch_tx: mpsc::Sender<SourceChangeEvent>,
    ) {
        while let Some(packet) = rx.recv().await {
            // generate source changes from the packet and topic name
            let source_changes = Self::process(&matcher, &packet);

            // send to the batcher
            Self::send_to_batcher(&source_id.clone(), &batch_tx, source_changes).await;
        }

        info!("[{source_id}] MQTT processing loop stopped - source channel closed");
    }

    fn process(mapper: &PatternMatcher, packet: &MqttPacket) -> Vec<MqttSourceChange> {
        match mapper.generate_schema(packet) {
            Ok(changes) => changes,
            Err(e) => {
                error!(
                    "Error processing Mqtt packet with topic {}: {}",
                    packet.topic, e
                );
                vec![]
            }
        }
    }

    async fn send_to_batcher(
        source_id: &str,
        batch_tx: &mpsc::Sender<SourceChangeEvent>,
        source_changes: Vec<MqttSourceChange>,
    ) {
        let source_id = source_id.to_string();
        for (idx, change) in source_changes.into_iter().enumerate() {
            let timestamp = match change {
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
                    };

                    if let Err(e) = batch_tx.send(change_event).await {
                        error!(
                            "[{source_id}] Failed to send change event to batcher for change {idx}: {e}"
                        );
                    } else {
                        debug!(
                            "[{source_id}] Successfully sent change event to batcher for change {idx}",
                        );
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
