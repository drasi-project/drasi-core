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

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::models::{Element, SourceChange};
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::computation::v1::{
    ComputationBootstrapProvider, ComputationBootstrapSnapshot, GraphChangeCodec, StreamId,
};

/// Bootstrap provider for application sources that replays stored insert events
pub struct ApplicationBootstrapProvider {
    /// Shared reference to bootstrap data (insert events) for replay
    /// Application code maintains this independently of live source input.
    bootstrap_data: Arc<RwLock<Vec<SourceChange>>>,
    stream: StreamId,
    node_labels: Vec<String>,
    relation_labels: Vec<String>,
}

impl ApplicationBootstrapProvider {
    /// Create a new provider with its own isolated bootstrap data storage
    /// Note: This creates an independent storage that won't be connected to any ApplicationSource
    pub fn new() -> Self {
        Self::with_shared_data(Arc::new(RwLock::new(Vec::new())))
    }

    /// Create a provider over application-owned snapshot data.
    pub fn with_shared_data(bootstrap_data: Arc<RwLock<Vec<SourceChange>>>) -> Self {
        Self {
            bootstrap_data,
            stream: StreamId::try_new("application/bootstrap").expect("constant stream"),
            node_labels: Vec::new(),
            relation_labels: Vec::new(),
        }
    }

    /// Select the native snapshot stream. It is not a live-source checkpoint.
    pub fn with_stream(mut self, stream: StreamId) -> Self {
        self.stream = stream;
        self
    }

    /// Limit native snapshots to nodes with any selected label. Empty means all nodes.
    pub fn with_node_labels(mut self, labels: Vec<String>) -> Self {
        self.node_labels = labels;
        self
    }

    /// Limit native snapshots to relations with any selected label. Empty means all relations.
    pub fn with_relation_labels(mut self, labels: Vec<String>) -> Self {
        self.relation_labels = labels;
        self
    }

    /// Create a builder for ApplicationBootstrapProvider
    pub fn builder() -> ApplicationBootstrapProviderBuilder {
        ApplicationBootstrapProviderBuilder::new()
    }
}

impl Default for ApplicationBootstrapProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for ApplicationBootstrapProvider
///
/// # Example
///
/// ```no_run
/// use drasi_bootstrap_application::ApplicationBootstrapProvider;
///
/// // Create with isolated storage
/// let provider = ApplicationBootstrapProvider::builder().build();
///
/// // Create with shared storage
/// use std::sync::Arc;
/// use tokio::sync::RwLock;
/// use drasi_core::models::SourceChange;
///
/// let shared_data = Arc::new(RwLock::new(Vec::<SourceChange>::new()));
/// let provider = ApplicationBootstrapProvider::builder()
///     .with_shared_data(shared_data)
///     .build();
/// ```
pub struct ApplicationBootstrapProviderBuilder {
    shared_data: Option<Arc<RwLock<Vec<SourceChange>>>>,
}

impl ApplicationBootstrapProviderBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self { shared_data: None }
    }

    /// Set shared bootstrap data
    ///
    /// The application maintains this independently of live source input.
    pub fn with_shared_data(mut self, data: Arc<RwLock<Vec<SourceChange>>>) -> Self {
        self.shared_data = Some(data);
        self
    }

    /// Build the ApplicationBootstrapProvider
    pub fn build(self) -> ApplicationBootstrapProvider {
        match self.shared_data {
            Some(data) => ApplicationBootstrapProvider::with_shared_data(data),
            None => ApplicationBootstrapProvider::new(),
        }
    }
}

impl Default for ApplicationBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplicationBootstrapProvider {
    /// Store an insert event for future bootstrap replay
    /// Application code explicitly maintains this data; live input does not update it.
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
    fn matches_labels(
        change: &SourceChange,
        node_labels: &[String],
        relation_labels: &[String],
    ) -> bool {
        match change {
            SourceChange::Insert { element } | SourceChange::Update { element, .. } => {
                match element {
                    Element::Node { metadata, .. } => {
                        node_labels.is_empty()
                            || metadata.labels.iter().any(|label| {
                                node_labels
                                    .iter()
                                    .any(|requested| requested == label.as_ref())
                            })
                    }
                    Element::Relation { metadata, .. } => {
                        relation_labels.is_empty()
                            || metadata.labels.iter().any(|label| {
                                relation_labels
                                    .iter()
                                    .any(|requested| requested == label.as_ref())
                            })
                    }
                }
            }
            SourceChange::Delete { metadata } => {
                node_labels.is_empty() && relation_labels.is_empty()
                    || metadata.labels.iter().any(|l| {
                        node_labels
                            .iter()
                            .chain(relation_labels)
                            .any(|requested| requested == l.as_ref())
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
impl ComputationBootstrapProvider for ApplicationBootstrapProvider {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let events = self.get_stored_events().await;
        let stream = self.stream.clone();
        let node_labels = self.node_labels.clone();
        let relation_labels = self.relation_labels.clone();
        let changes = events
            .into_iter()
            .filter(move |change| Self::matches_labels(change, &node_labels, &relation_labels))
            .enumerate()
            .map(move |(index, change)| {
                let sequence = u64::try_from(index)?
                    .checked_add(1)
                    .context("application bootstrap sequence exhausted")?;
                let timestamp = chrono::DateTime::from_timestamp_millis(i64::try_from(
                    change.get_transaction_time(),
                )?)
                .context("application bootstrap timestamp cannot be represented")?;
                Ok::<_, anyhow::Error>(GraphChangeCodec::encode_change(
                    change,
                    stream.clone(),
                    sequence,
                    Some(timestamp),
                )?)
            });
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(tokio_stream::iter(changes)),
            watermarks: Vec::new(),
        })
    }
}
