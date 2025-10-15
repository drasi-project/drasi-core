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

//! Application bootstrap provider for replaying stored insert events

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{Element, SourceChange};
use log::{error, info};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::bootstrap::{BootstrapContext, BootstrapProvider};
use crate::channels::{BootstrapRequest, SourceEvent, SourceEventWrapper};

/// Bootstrap provider for application sources that replays stored insert events
pub struct ApplicationBootstrapProvider {
    /// Stores bootstrap data (insert events) for replay
    bootstrap_data: Arc<RwLock<Vec<SourceChange>>>,
}

impl ApplicationBootstrapProvider {
    pub fn new() -> Self {
        Self {
            bootstrap_data: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Store an insert event for future bootstrap replay
    /// This would be called by the application source when it receives insert events
    pub async fn store_insert_event(&self, change: SourceChange) {
        if matches!(change, SourceChange::Insert { .. }) {
            self.bootstrap_data.write().await.push(change);
        }
    }

    /// Get all stored insert events (for testing or inspection)
    pub async fn get_stored_events(&self) -> Vec<SourceChange> {
        self.bootstrap_data.read().await.clone()
    }

    /// Clear stored events (for testing or reset)
    pub async fn clear_stored_events(&self) {
        self.bootstrap_data.write().await.clear();
    }

    /// Check if a change matches the requested labels
    fn matches_labels(&self, change: &SourceChange, request: &BootstrapRequest) -> bool {
        match change {
            SourceChange::Insert { element } | SourceChange::Update { element, .. } => {
                match element {
                    Element::Node { metadata, .. } => {
                        request.node_labels.is_empty()
                            || metadata
                                .labels
                                .iter()
                                .any(|l| request.node_labels.contains(&l.to_string()))
                    }
                    Element::Relation { metadata, .. } => {
                        request.relation_labels.is_empty()
                            || metadata
                                .labels
                                .iter()
                                .any(|l| request.relation_labels.contains(&l.to_string()))
                    }
                }
            }
            SourceChange::Delete { metadata } => {
                request.node_labels.is_empty() && request.relation_labels.is_empty()
                    || metadata.labels.iter().any(|l| {
                        request.node_labels.contains(&l.to_string())
                            || request.relation_labels.contains(&l.to_string())
                    })
            }
            SourceChange::Future { .. } => {
                // Future events are not supported in bootstrap
                false
            }
        }
    }
}

#[async_trait]
impl BootstrapProvider for ApplicationBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
    ) -> Result<usize> {
        info!(
            "ApplicationBootstrapProvider processing bootstrap request for query '{}' with {} node labels and {} relation labels",
            request.query_id,
            request.node_labels.len(),
            request.relation_labels.len()
        );

        let bootstrap_data = self.bootstrap_data.read().await;
        let mut count = 0;

        if bootstrap_data.is_empty() {
            info!(
                "ApplicationBootstrapProvider: No stored events to replay for query '{}'",
                request.query_id
            );
            return Ok(0);
        }

        info!(
            "ApplicationBootstrapProvider: Replaying {} stored events for query '{}'",
            bootstrap_data.len(),
            request.query_id
        );

        for change in bootstrap_data.iter() {
            // Filter by requested labels
            if self.matches_labels(change, &request) {
                let wrapper = SourceEventWrapper {
                    source_id: context.source_id.clone(),
                    event: SourceEvent::Change(change.clone()),
                    timestamp: chrono::Utc::now(),
                };

                if let Err(e) = context.source_event_tx.send(wrapper).await {
                    error!("Failed to send bootstrap event: {}", e);
                    break;
                }
                count += 1;
            }
        }

        info!(
            "ApplicationBootstrapProvider sent {} bootstrap events for query '{}'",
            count, request.query_id
        );
        Ok(count)
    }
}

// Note: In a full implementation, we would need a way for the ApplicationSource
// to share its bootstrap data with this provider. This could be done through:
// 1. A shared data structure passed during provider creation
// 2. A registry where sources register their bootstrap data
// 3. Integration through the server core that connects them
//
// For now, this provider maintains its own bootstrap data storage,
// but in practice it would need to be connected to the actual ApplicationSource's
// bootstrap_data field.
