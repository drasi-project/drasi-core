// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

//! Sends Temporal workflow commands (signal, start, update, cancel, terminate) in
//! response to query result additions, updates, and removals. Each operation must
//! be mapped to an action explicitly; unmapped operations send nothing.
//!
//! ```no_run
//! use drasi_reaction_temporal::{TemporalReaction, TemporalReactionConfig};
//! # fn main() -> anyhow::Result<()> {
//! let config: TemporalReactionConfig = serde_json::from_value(serde_json::json!({
//!     "outputTemplates": { "routes": { "alerts": {
//!         "added": { "action": "signal", "workflowId": "monitor",
//!                    "signalName": "alert", "template": "{{json after}}" }
//!     }}}
//! }))?;
//! let reaction = TemporalReaction::builder("temporal")
//!     .with_query("alerts").with_config(config).build()?;
//! # Ok(()) }
//! ```
//!
//! The host supplies persistent query output and a durable reaction state store.
//! Nothing starts a workflow unless an operation explicitly selects `start`.

#![allow(unexpected_cfgs)]

mod builder;
mod client;
pub mod config;
pub mod descriptor;
mod reaction;
mod render;
mod store;

pub use builder::TemporalReactionBuilder;
pub use config::{
    Action, ActionSpec, ClientIdentity, ConflictPolicy, OutputTemplates, QueryActions, ReusePolicy,
    TemporalReactionConfig, TlsConfig, UpdateWait,
};
pub use reaction::TemporalReaction;

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "temporal-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::TemporalReactionDescriptor],
    bootstrap_descriptors = [],
);

#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
