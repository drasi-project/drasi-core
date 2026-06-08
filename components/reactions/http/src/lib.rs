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
//! by adding an optional [`config::AdaptiveBatchConfig`]. See
//! [`config::HttpReactionConfig`] for the full configuration shape and the
//! crate `README.md` for examples.
//!
//! ## Quick start (per-result)
//!
//! ```rust,ignore
//! use drasi_reaction_http::{HttpReaction, HttpReactionConfig};
//!
//! let reaction = HttpReaction::builder("my-http")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_base_url("https://api.example.com")
//!     .with_token("secret")
//!     .build()?;
//! ```
//!
//! ## Quick start (adaptive batching)
//!
//! ```rust,ignore
//! use drasi_reaction_http::{AdaptiveBatchConfig, HttpReaction};
//!
//! let reaction = HttpReaction::builder("my-http")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_base_url("https://api.example.com")
//!     .with_adaptive(AdaptiveBatchConfig::default())
//!     .with_batch_endpoint("/batch")
//!     .build()?;
//! ```

pub(crate) mod adaptive_batcher;
pub(crate) mod adaptive_loop;
pub(crate) mod batch;
pub mod config;
pub mod descriptor;
pub mod http;
pub(crate) mod process;
pub(crate) mod standard_loop;

#[cfg(test)]
mod tests;

pub use batch::BatchResult;
pub use config::{
    AdaptiveBatchConfig, HttpCallExt, HttpCallSpec, HttpOutputTemplates, HttpQueryConfig,
    HttpReactionConfig, OperationType, QueryConfig, TemplateRouting, TemplateSpec,
};
pub use http::HttpReaction;

/// Builder for the HTTP reaction.
///
/// Provides a fluent API for the common cases. For complex template
/// routes you can also pass a fully-formed [`HttpOutputTemplates`] or
/// [`HttpReactionConfig`].
pub struct HttpReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: HttpReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl HttpReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: HttpReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
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

    /// Set the minimum adaptive batch size.
    ///
    /// **Side effect:** calling this enables adaptive mode if it is not
    /// already enabled (creating a default [`AdaptiveBatchConfig`]). To
    /// configure tuning without surprises, prefer
    /// [`with_adaptive`](Self::with_adaptive) with an explicit config.
    pub fn with_min_batch_size(mut self, n: usize) -> Self {
        let a = self
            .config
            .adaptive
            .get_or_insert_with(AdaptiveBatchConfig::default);
        a.adaptive_min_batch_size = n;
        self
    }

    /// Set the maximum adaptive batch size.
    ///
    /// **Side effect:** calling this enables adaptive mode if it is not
    /// already enabled (creating a default [`AdaptiveBatchConfig`]).
    pub fn with_max_batch_size(mut self, n: usize) -> Self {
        let a = self
            .config
            .adaptive
            .get_or_insert_with(AdaptiveBatchConfig::default);
        a.adaptive_max_batch_size = n;
        self
    }

    /// Set the adaptive throughput window size (in 100 ms units).
    ///
    /// **Side effect:** calling this enables adaptive mode if it is not
    /// already enabled (creating a default [`AdaptiveBatchConfig`]).
    pub fn with_window_size(mut self, n: usize) -> Self {
        let a = self
            .config
            .adaptive
            .get_or_insert_with(AdaptiveBatchConfig::default);
        a.adaptive_window_size = n;
        self
    }

    /// Set the adaptive batch flush timeout (milliseconds).
    ///
    /// **Side effect:** calling this enables adaptive mode if it is not
    /// already enabled (creating a default [`AdaptiveBatchConfig`]).
    pub fn with_batch_timeout_ms(mut self, ms: u64) -> Self {
        let a = self
            .config
            .adaptive
            .get_or_insert_with(AdaptiveBatchConfig::default);
        a.adaptive_batch_timeout_ms = ms;
        self
    }

    /// Set the batch endpoint path. Coalesced batches are POSTed to
    /// `{baseUrl}{endpoint}` as a single payload.
    ///
    /// Requires adaptive mode (enable it via [`with_adaptive`](Self::with_adaptive)
    /// or one of the `with_*_batch_*` tuning methods). If a batch endpoint is
    /// set without adaptive mode, [`HttpReaction::start`](crate::HttpReaction)
    /// fails fast at startup: it logs an error, sets the reaction status to
    /// `Stopped`, and returns an error.
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
        Ok(HttpReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
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
