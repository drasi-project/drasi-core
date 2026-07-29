#![allow(unexpected_cfgs)]
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

//! # State Machine source for Drasi
//!
//! A Drasi **source** that maps live continuous-query results to per-entity state
//! transitions and exposes the realtime state of every entity as a graph source
//! that downstream queries can subscribe to.
//!
//! Unlike an ordinary source that ingests data from an external system, this
//! source is driven by the results of other continuous queries. It uses the
//! source query-subscription API — [`drasi_lib::Source::subscribed_query_ids`]
//! and [`drasi_lib::Source::enqueue_query_result`] — the same way a reaction
//! subscribes to query results. Each [`config::EnterCondition`] declares a
//! query, the result `ops` that trigger it, the allowed `previous` states, and a
//! Handlebars `key` template that extracts the entity key from the result row.
//! When a result matches and the entity is in an allowed prior state, the entity
//! transitions; the new state is persisted to the state store and dispatched as a
//! node change to the source's own subscribers.
//!
//! Because the state machine is a single component, its own id **is** the source
//! id downstream queries subscribe to — there is no separate companion component.
//!
//! ## Example
//!
//! ```no_run
//! use drasi_source_state_machine::{StateMachineSourceBuilder, config::{StateDef, EnterCondition, Op}};
//!
//! # fn build() -> anyhow::Result<()> {
//! let source = StateMachineSourceBuilder::new("order-state-source")
//!     .with_entity_label("OrderState")
//!     .with_key_field("orderId")
//!     .with_state(StateDef {
//!         id: "NEW".to_string(),
//!         enter: vec![EnterCondition {
//!             query: "draft-orders".to_string(),
//!             previous: vec![],
//!             key: "{{orderId}}".to_string(),
//!             ops: vec![Op::Added],
//!         }],
//!     })
//!     .build()?;
//! // drasi.add_source(source).await?;
//! # let _ = source;
//! # Ok(())
//! # }
//! ```
//!
//! A **durable** `StateStoreProvider` (e.g. `drasi-state-store-redb`) should be
//! configured on the `DrasiLib` instance so entity state survives restarts.

pub mod config;
pub mod descriptor;
pub mod engine;
pub mod source;

#[cfg(test)]
mod tests;

pub use config::{EnterCondition, Op, StateDef, StateMachineSourceConfig};
pub use engine::{EntityRecord, StateMachine};
pub use source::StateMachineSource;

/// Builder for a [`StateMachineSource`].
pub struct StateMachineSourceBuilder {
    id: String,
    config: StateMachineSourceConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl StateMachineSourceBuilder {
    /// Start building a state machine source with the given id.
    ///
    /// This id is the source id downstream queries subscribe to.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: StateMachineSourceConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the label applied to emitted entity-state nodes.
    pub fn with_entity_label(mut self, label: impl Into<String>) -> Self {
        self.config.entity_label = label.into();
        self
    }

    /// Set the node property name that holds the entity key.
    pub fn with_key_field(mut self, key_field: impl Into<String>) -> Self {
        self.config.key_field = key_field.into();
        self
    }

    /// Add a state definition.
    pub fn with_state(mut self, state: StateDef) -> Self {
        self.config.states.push(state);
        self
    }

    /// Replace the entire configuration.
    pub fn with_config(mut self, config: StateMachineSourceConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the query-result priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the source auto-starts.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Build the source.
    pub fn build(self) -> anyhow::Result<StateMachineSource> {
        StateMachineSource::create(
            self.id,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}

/// Dynamic plugin entry point. Exports the state machine source descriptor.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "state-machine-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::StateMachineSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
