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

//! Source trait module
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
//! # Runtime Context Initialization
//!
//! Sources receive all drasi-lib services through a single `initialize()` call
//! when added to DrasiLib. The `SourceRuntimeContext` provides:
//! - `event_tx`: Channel for component lifecycle events
//! - `state_store`: Optional persistent state storage
//!
//! This replaces the previous `inject_*` methods with a cleaner single-call pattern.

use anyhow::Result;
use async_trait::async_trait;

use crate::channels::*;
use crate::context::SourceRuntimeContext;

/// Trait defining the interface for all source implementations.
///
/// This is the core abstraction that all source plugins must implement.
/// drasi-lib only interacts with sources through this trait - it has no
/// knowledge of specific plugin types or their configurations.
///
/// # Example Implementation
///
/// ```ignore
/// use drasi_lib::Source;
/// use drasi_lib::sources::{SourceBase, SourceBaseParams};
/// use drasi_lib::context::SourceRuntimeContext;
///
/// pub struct MySource {
///     base: SourceBase,
///     // Plugin-specific fields
/// }
///
/// impl MySource {
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
///     async fn initialize(&self, context: SourceRuntimeContext) {
///         self.base.initialize(context).await;
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

    /// Whether this source should auto-start when DrasiLib starts
    ///
    /// Default is `true`. Override to return `false` if this source
    /// should only be started manually via `start_source()`.
    fn auto_start(&self) -> bool {
        true
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
    /// * `settings` - Subscription settings including query ID, text, and labels of interest
    ///
    /// # Returns
    /// A SubscriptionResponse containing:
    /// * A receiver for streaming source events
    /// * Optionally a bootstrap receiver for initial data
    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse>;

    /// Downcast helper for testing - allows access to concrete types
    fn as_any(&self) -> &dyn std::any::Any;

    /// Initialize the source with runtime context.
    ///
    /// This method is called automatically by DrasiLib when the source is added
    /// via `add_source()`. Plugin developers do not need to call this directly.
    ///
    /// The context provides access to:
    /// - `source_id`: The source's unique identifier
    /// - `event_tx`: Channel for reporting component lifecycle events
    /// - `state_store`: Optional persistent state storage
    ///
    /// Implementation should delegate to `self.base.initialize(context).await`.
    async fn initialize(&self, context: SourceRuntimeContext);

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

/// Blanket implementation of Source for `Box<dyn Source>`
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

    fn auto_start(&self) -> bool {
        (**self).auto_start()
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
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        (**self).subscribe(settings).await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        (**self).as_any()
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        (**self).initialize(context).await
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn crate::bootstrap::BootstrapProvider + 'static>,
    ) {
        (**self).set_bootstrap_provider(provider).await
    }
}
