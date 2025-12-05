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

//! Plugin core module for source abstractions
//!
//! This module provides the core trait that all source plugins must implement.
//! It separates the plugin contract from the source manager and implementation details.
//!
//! # Plugin Architecture
//!
//! Each source plugin:
//! 1. Defines its own typed configuration struct with builder pattern
//! 2. Creates a `SourceBase` instance using `SourceBaseParams`
//! 3. Implements the `Source` trait
//! 4. Is passed to `DrasiLib` via `add_source()` which takes ownership
//!
//! drasi-lib has no knowledge of which plugins exist - it only knows about this trait.
//!
//! # Event Channel Injection
//!
//! Sources do not need to receive an event channel during construction.
//! DrasiLib automatically injects the event channel when the source is added
//! via `add_source()`. This simplifies the plugin constructor API.

use anyhow::Result;
use async_trait::async_trait;

use crate::channels::*;

/// Trait defining the interface for all source implementations.
///
/// This is the core abstraction that all source plugins must implement.
/// drasi-lib only interacts with sources through this trait - it has no
/// knowledge of specific plugin types or their configurations.
///
/// # Example Implementation
///
/// ```ignore
/// use drasi_lib::plugin_core::Source;
/// use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
///
/// pub struct MySource {
///     base: SourceBase,
///     // Plugin-specific fields
/// }
///
/// impl MySource {
///     // No event_tx needed - it's injected automatically by DrasiLib
///     pub fn new(config: MySourceConfig) -> Result<Self> {
///         let params = SourceBaseParams::new(&config.id)
///             .with_dispatch_mode(config.dispatch_mode)
///             .with_dispatch_buffer_capacity(config.buffer_capacity);
///
///         Ok(Self {
///             base: SourceBase::new(params)?,
///         })
///     }
/// }
///
/// #[async_trait]
/// impl Source for MySource {
///     fn id(&self) -> &str {
///         &self.base.id
///     }
///
///     fn type_name(&self) -> &str {
///         "my-source"
///     }
///
///     fn properties(&self) -> HashMap<String, Value> {
///         // Return plugin-specific properties
///     }
///
///     async fn inject_event_tx(&self, tx: ComponentEventSender) {
///         self.base.inject_event_tx(tx).await;
///     }
///
///     // ... implement other methods
/// }
/// ```
#[async_trait]
pub trait Source: Send + Sync {
    /// Get the source's unique identifier
    fn id(&self) -> &str;

    /// Get the source type name (e.g., "postgres", "http", "mock")
    fn type_name(&self) -> &str;

    /// Get the source's configuration properties for inspection
    ///
    /// This returns a HashMap representation of the source's configuration
    /// for use in APIs and inspection. The actual typed configuration is
    /// owned by the plugin - this is just for external visibility.
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value>;

    /// Get the dispatch mode for this source (Channel or Broadcast)
    ///
    /// Default is Channel mode for backpressure support.
    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    /// Start the source
    ///
    /// This begins data ingestion and event generation.
    async fn start(&self) -> Result<()>;

    /// Stop the source
    ///
    /// This stops data ingestion and cleans up resources.
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the source
    async fn status(&self) -> ComponentStatus;

    /// Subscribe to this source for change events
    ///
    /// This is called by queries to receive data changes from this source.
    /// The source should return a receiver for streaming events and optionally
    /// a bootstrap receiver for initial data.
    ///
    /// # Arguments
    /// * `query_id` - ID of the subscribing query
    /// * `enable_bootstrap` - Whether to request initial data
    /// * `node_labels` - Node labels the query is interested in (for filtering)
    /// * `relation_labels` - Relation labels the query is interested in
    ///
    /// # Returns
    /// A SubscriptionResponse containing:
    /// * A receiver for streaming source events
    /// * Optionally a bootstrap receiver for initial data
    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse>;

    /// Downcast helper for testing - allows access to concrete types
    fn as_any(&self) -> &dyn std::any::Any;

    /// Inject the event channel for component lifecycle events
    ///
    /// This method is called automatically by DrasiLib when the source is added
    /// via `add_source()`. Plugin developers do not need to call this directly.
    ///
    /// Implementation should delegate to `self.base.inject_event_tx(tx).await`.
    async fn inject_event_tx(&self, tx: ComponentEventSender);

    /// Set the bootstrap provider for this source
    ///
    /// This method allows setting a bootstrap provider after source construction.
    /// It is optional - sources without a bootstrap provider will report that
    /// bootstrap is not available.
    ///
    /// Implementation should delegate to `self.base.set_bootstrap_provider(provider).await`.
    async fn set_bootstrap_provider(
        &self,
        _provider: Box<dyn crate::bootstrap::BootstrapProvider + 'static>,
    ) {
        // Default implementation does nothing - sources that support bootstrap
        // should override this to delegate to their SourceBase
    }
}

/// Blanket implementation of Source for Box<dyn Source>
///
/// This allows boxed trait objects to be used with methods expecting `impl Source`.
#[async_trait]
impl Source for Box<dyn Source + 'static> {
    fn id(&self) -> &str {
        (**self).id()
    }

    fn type_name(&self) -> &str {
        (**self).type_name()
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        (**self).properties()
    }

    fn dispatch_mode(&self) -> DispatchMode {
        (**self).dispatch_mode()
    }

    async fn start(&self) -> Result<()> {
        (**self).start().await
    }

    async fn stop(&self) -> Result<()> {
        (**self).stop().await
    }

    async fn status(&self) -> ComponentStatus {
        (**self).status().await
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        (**self)
            .subscribe(query_id, enable_bootstrap, node_labels, relation_labels)
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        (**self).as_any()
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        (**self).inject_event_tx(tx).await
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn crate::bootstrap::BootstrapProvider + 'static>,
    ) {
        (**self).set_bootstrap_provider(provider).await
    }
}

