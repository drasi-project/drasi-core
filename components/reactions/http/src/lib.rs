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

#![allow(unexpected_cfgs)]

//! HTTP reaction plugin for Drasi.
//!
//! Forwards continuous query result changes to HTTP endpoints, either as
//! **single notifications per result** (default) or **batched** using an
//! **adaptive batching** strategy that scales batch sizes to incoming load
//! for greater throughput and reduced network traffic. Batching is enabled
//! by adding [`config::AdaptiveBatchConfig`] and a batch endpoint. See
//! [`config::HttpReactionConfig`] for the full configuration shape and the
//! crate `README.md` for examples.
//!
//! ## Quick start (per-result)
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use drasi_reaction_http::HttpReaction;
//! use drasi_lib::{ComponentId, ComponentName};
//!
//! let reaction = HttpReaction::builder(ComponentId::try_new(
//!     "a8c21940-3d72-4f61-923b-04a8ed651902",
//! )?)
//!     .with_query_by_name(ComponentName::try_new("query1")?)
//!     .with_base_url("https://api.example.com")
//!     .with_token("secret")
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```
//!
//! ## Quick start (adaptive batching)
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use drasi_reaction_http::{AdaptiveBatchConfig, HttpReaction};
//! use drasi_lib::{ComponentId, ComponentName};
//!
//! let reaction = HttpReaction::builder(ComponentId::try_new(
//!     "a8c21940-3d72-4f61-923b-04a8ed651902",
//! )?)
//!     .with_query_by_name(ComponentName::try_new("query1")?)
//!     .with_base_url("https://api.example.com")
//!     .with_adaptive(AdaptiveBatchConfig::default())
//!     .with_batch_endpoint("/batch")
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```

pub(crate) mod adaptive_batcher;
pub(crate) mod adaptive_loop;
pub(crate) use drasi_http_common::reaction::{batch, process};
pub use drasi_http_common::reaction::{config, output};
pub mod descriptor;
pub mod http;
pub(crate) mod standard_loop;

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;

pub use config::{
    AdaptiveBatchConfig, HttpCallExt, HttpCallSpec, HttpOutputTemplates, HttpQueryConfig,
    HttpReactionConfig, OperationType, QueryConfig, TemplateRouting, TemplateSpec,
};
use drasi_lib::recovery::ReactionRecoveryPolicy;
pub use http::HttpReaction;
pub use output::{BatchEnvelope, DefaultChangeNotification, Operation};

/// Builder for the HTTP reaction.
///
/// Provides a fluent API for the common cases. For complex template
/// routes you can also pass a fully-formed [`HttpOutputTemplates`] or
/// [`HttpReactionConfig`].
pub struct HttpReactionBuilder {
    id: drasi_lib::ComponentId,
    queries: Vec<drasi_lib::config::ComponentReference>,
    config: HttpReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl HttpReactionBuilder {
    pub fn new(id: drasi_lib::ComponentId) -> Self {
        Self {
            id: id.clone(),
            queries: Vec::new(),
            config: HttpReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
            recovery_policy: None,
        }
    }

    /// Set explicit name or ID references for every query input.
    pub fn with_query_inputs(mut self, inputs: Vec<drasi_lib::config::ComponentReference>) -> Self {
        self.queries = inputs;
        self
    }

    /// Set all query inputs by their current names.
    pub fn with_queries_by_name(self, names: Vec<drasi_lib::ComponentName>) -> Self {
        self.with_query_inputs(
            names
                .into_iter()
                .map(drasi_lib::config::ComponentReference::by_name)
                .collect(),
        )
    }

    /// Set all query inputs by their immutable IDs.
    pub fn with_queries_by_id(self, ids: Vec<drasi_lib::ComponentId>) -> Self {
        self.with_query_inputs(
            ids.into_iter()
                .map(drasi_lib::config::ComponentReference::by_id)
                .collect(),
        )
    }

    /// Add a query input by its current name.
    pub fn with_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn with_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
        self
    }

    /// Add a query input by its current name.
    pub fn from_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn from_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.base_url = base_url.into();
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.config.token = Some(token.into());
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.timeout_ms = timeout_ms;
        self
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_recovery_policy(mut self, policy: ReactionRecoveryPolicy) -> Self {
        self.recovery_policy = Some(policy);
        self
    }

    /// Replace the default fallback template applied to every query that
    /// has no explicit route.
    pub fn with_default_template(mut self, template: HttpQueryConfig) -> Self {
        let templates = self
            .config
            .output_templates
            .get_or_insert_with(Default::default);
        templates.default_template = Some(template);
        self
    }

    /// Add or replace the per-query template for `query_id`.
    pub fn with_query_template(
        mut self,
        query_id: impl Into<String>,
        template: HttpQueryConfig,
    ) -> Self {
        let templates = self
            .config
            .output_templates
            .get_or_insert_with(Default::default);
        templates.routes.insert(query_id.into(), template);
        self
    }

    /// Replace the whole [`HttpOutputTemplates`] block at once.
    pub fn with_output_templates(mut self, templates: HttpOutputTemplates) -> Self {
        self.config.output_templates = Some(templates);
        self
    }

    /// Enable adaptive batching with the given [`AdaptiveBatchConfig`].
    pub fn with_adaptive(mut self, adaptive: AdaptiveBatchConfig) -> Self {
        self.config.adaptive = Some(adaptive);
        self
    }

    /// Convenience: enable adaptive batching with default tuning.
    pub fn with_adaptive_defaults(self) -> Self {
        self.with_adaptive(AdaptiveBatchConfig::default())
    }

    /// Set the batch endpoint path. Coalesced batches are POSTed to
    /// `{baseUrl}{endpoint}` as a single [`BatchEnvelope`] payload.
    ///
    /// Requires adaptive mode (enable it via [`with_adaptive`](Self::with_adaptive)).
    /// If a batch endpoint is set without adaptive mode, [`build`](Self::build)
    /// fails fast.
    pub fn with_batch_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.batch_endpoint = Some(endpoint.into());
        self
    }

    /// Replace the full configuration.
    pub fn with_config(mut self, config: HttpReactionConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> anyhow::Result<HttpReaction> {
        self.config.validate(
            &drasi_lib::reactions::common::input_aliases(&self.queries),
            self.priority_queue_capacity,
        )?;
        Ok(HttpReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
            self.recovery_policy,
        ))
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "http-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::HttpReactionDescriptor],
    bootstrap_descriptors = [],
);
