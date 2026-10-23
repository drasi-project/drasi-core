// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native Durable Task Scheduler reaction.
//!
//! Operators select start, external-event, terminate or ignore actions for each
//! query's ADD/UPDATE/DELETE results. No bridge service is required. This targets
//! DTS, not arbitrary classic Durable Task Framework storage providers.
//!
//! ```no_run
//! use drasi_reaction_durable_task::{
//!     Action, DurableTaskReaction, DurableTaskQueryConfig, DurableTaskTemplate, TargetPolicy,
//! };
//! # fn main() -> anyhow::Result<()> {
//! let reaction = DurableTaskReaction::builder("workflows")
//!     .with_query("orders")
//!     .with_endpoint("https://scheduler.example.com")
//!     .with_task_hub("orders")
//!     .with_producer_id("orders-production-v1")
//!     .with_default_template(DurableTaskQueryConfig {
//!         added: Some(DurableTaskTemplate::with_extension("", Action::Start {
//!             orchestration: "OrderWorkflow".into(),
//!             version: String::new(),
//!             instance_id: Some("order-{{after.orderId}}".into()),
//!             on_conflict: TargetPolicy::Error,
//!         })),
//!         ..Default::default()
//!     })
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```

mod client;
pub mod config;
pub mod descriptor;
mod dispatch;
mod reaction;

pub use config::{
    Action, Authentication, DurableTaskQueryConfig, DurableTaskReactionConfig, DurableTaskTemplate,
    TargetPolicy,
};
pub use reaction::DurableTaskReaction;

// Names and generated server/client APIs follow the unmodified upstream schema.
#[allow(clippy::enum_variant_names)]
mod proto {
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}

use drasi_lib::reactions::common::base::ReactionBaseParams;
use drasi_lib::recovery::ReactionRecoveryPolicy;

/// Builder shared by embedded applications and the dynamic-plugin descriptor.
pub struct DurableTaskReactionBuilder {
    params: ReactionBaseParams,
    config: DurableTaskReactionConfig,
}

impl DurableTaskReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            params: ReactionBaseParams::new(id, Vec::new()),
            config: DurableTaskReactionConfig::default(),
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.params.queries = queries;
        self
    }
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.params.queries.push(query.into());
        self
    }
    pub fn from_query(self, query: impl Into<String>) -> Self {
        self.with_query(query)
    }
    pub fn with_config(mut self, config: DurableTaskReactionConfig) -> Self {
        self.config = config;
        self
    }
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.endpoint = endpoint.into();
        self
    }
    pub fn with_task_hub(mut self, task_hub: impl Into<String>) -> Self {
        self.config.task_hub = task_hub.into();
        self
    }
    pub fn with_producer_id(mut self, producer_id: impl Into<String>) -> Self {
        self.config.producer_id = producer_id.into();
        self
    }
    pub fn with_authentication(mut self, authentication: Authentication) -> Self {
        self.config.authentication = authentication;
        self
    }
    pub fn with_default_template(mut self, template: DurableTaskQueryConfig) -> Self {
        self.config.default_template = Some(template);
        self
    }
    pub fn with_route(mut self, query: impl Into<String>, config: DurableTaskQueryConfig) -> Self {
        self.config.routes.insert(query.into(), config);
        self
    }
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.timeout_ms = timeout_ms;
        self
    }
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.config.max_retries = retries;
        self
    }
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.params.priority_queue_capacity = Some(capacity);
        self
    }
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.params.auto_start = auto_start;
        self
    }
    pub fn with_recovery_policy(mut self, policy: ReactionRecoveryPolicy) -> Self {
        self.params.recovery_policy = Some(policy);
        self
    }
    pub fn build(self) -> anyhow::Result<DurableTaskReaction> {
        anyhow::ensure!(
            !self.params.id.trim().is_empty(),
            "reaction ID must not be empty"
        );
        anyhow::ensure!(
            self.params.recovery_policy != Some(ReactionRecoveryPolicy::AutoReset),
            "AutoReset is not supported: resetting query sequences requires a new producerId"
        );
        self.config
            .validate(&self.params.queries, self.params.priority_queue_capacity)?;
        Ok(DurableTaskReaction::from_builder(self.params, self.config))
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "durable-task-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::DurableTaskReactionDescriptor],
    bootstrap_descriptors = [],
);

#[cfg(test)]
mod tests;
