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
use log::info;
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::bootstrap::BootstrapRequest;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider};

/// Bootstrap provider for application sources that replays stored insert events
pub struct ApplicationBootstrapProvider {
    /// Shared reference to bootstrap data (insert events) for replay
    /// This should be connected to ApplicationSource's bootstrap_data via shared Arc
    bootstrap_data: Arc<RwLock<Vec<SourceChange>>>,
}

impl ApplicationBootstrapProvider {
    /// Create a new provider with its own isolated bootstrap data storage
    /// Note: This creates an independent storage that won't be connected to any ApplicationSource
    pub fn new() -> Self {
        Self {
            bootstrap_data: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Create a new provider with a shared reference to ApplicationSource's bootstrap data
    /// This allows the provider to access the actual data stored by ApplicationSource
    pub fn with_shared_data(bootstrap_data: Arc<RwLock<Vec<SourceChange>>>) -> Self {
        Self { bootstrap_data }
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
    /// Use this when you want the provider to share bootstrap data with an ApplicationSource.
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
        _context: &BootstrapContext,
        _event_tx: drasi_lib::channels::BootstrapEventSender,
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
                // Note: Sequence numbering and profiling metadata are handled by
                // ApplicationSource.subscribe() which sends bootstrap events through
                // dedicated channels. This provider only counts matching events.
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

// Implementation Note: Bootstrap Data Connection
//
// This provider can be connected to ApplicationSource's actual bootstrap data in two ways:
//
// 1. Via with_shared_data() constructor:
//    When creating an ApplicationSource, pass the bootstrap_data Arc to the provider:
//    ```rust
//    let (source, handle) = ApplicationSource::new(config, event_tx);
//    let provider = ApplicationBootstrapProvider::with_shared_data(
//        source.get_bootstrap_data()
//    );
//    ```
//
// 2. Via BootstrapContext properties:
//    Store the bootstrap_data Arc in the source config properties as a special internal
//    property that can be retrieved by the provider during bootstrap operations.
//
// Currently, ApplicationSource handles bootstrap directly in its subscribe() method
// (lines 337-384 in sources/application/mod.rs), so this provider is not actively used
// by ApplicationSource. The provider exists for testing and potential future integration
// where bootstrap logic might be delegated to the provider system.
