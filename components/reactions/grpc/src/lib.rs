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

//! gRPC reaction plugin for Drasi.
//!
//! Forwards continuous query result changes to a downstream gRPC service via
//! the `ReactionService.ProcessResults` RPC, either with **fixed-size
//! batching** (default) or **adaptive batching** that scales batch size with
//! observed throughput. An optional Handlebars-based output template engine
//! can reshape the row content emitted in the `before` / `after` fields; when
//! no template applies (or a render fails) the raw row state is sent on the
//! wire unchanged so receivers always recover the change.

pub(crate) mod adaptive_batcher;
pub(crate) mod batch;
pub mod config;
pub mod connection;
pub mod descriptor;
pub mod grpc;
pub mod helpers;
pub mod proto;
mod runner_adaptive;
mod runner_fixed;
mod send;
mod templates;

pub use config::{
    BatchingConfig, GrpcQueryConfig, GrpcReactionConfig, GrpcTemplateExtension, OutputFormat,
    OutputTemplates,
};
pub use grpc::GrpcReaction;

pub use helpers::convert_json_to_proto_struct;
pub use proto::{
    ProcessResultsRequest, ProtoQueryResult, ProtoQueryResultItem, ReactionServiceClient,
};

use std::collections::HashMap;

/// Builder for [`GrpcReaction`].
pub struct GrpcReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: GrpcReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl GrpcReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: GrpcReactionConfig::default(),
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

    /// Alias of [`with_query`](Self::with_query); reads naturally at call
    /// sites (e.g. `…from_query("orders")…`).
    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.endpoint = endpoint.into();
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

    pub fn with_connection_retry_attempts(mut self, attempts: u32) -> Self {
        self.config.connection_retry_attempts = attempts;
        self
    }

    pub fn with_initial_connection_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.initial_connection_timeout_ms = timeout_ms;
        self
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.metadata.insert(key.into(), value.into());
        self
    }

    pub fn with_all_metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.config.metadata = metadata;
        self
    }

    pub fn with_batching(mut self, batching: BatchingConfig) -> Self {
        self.config.batching = batching;
        self
    }

    pub fn with_output_format(mut self, output_format: OutputFormat) -> Self {
        self.config.output_format = output_format;
        self
    }

    pub fn with_fixed_batching(self, batch_size: usize, batch_flush_timeout_ms: u64) -> Self {
        self.with_batching(BatchingConfig::Fixed {
            batch_size,
            batch_flush_timeout_ms,
        })
    }

    pub fn with_adaptive_batching(
        self,
        adaptive: drasi_lib::reactions::common::AdaptiveBatchConfig,
    ) -> Self {
        self.with_batching(BatchingConfig::adaptive(adaptive))
    }

    /// Convenience: enable adaptive batching with default tuning.
    pub fn with_adaptive_defaults(self) -> Self {
        self.with_adaptive_batching(drasi_lib::reactions::common::AdaptiveBatchConfig::default())
    }

    /// Set the minimum adaptive batch size.
    ///
    /// **Side effect:** calling this enables adaptive mode if the builder
    /// currently has `BatchingConfig::Fixed` (replacing it with an adaptive
    /// config whose other fields take their defaults). For explicit control
    /// over all adaptive parameters, prefer
    /// [`with_adaptive_batching`](Self::with_adaptive_batching) with a full
    /// `AdaptiveBatchConfig`.
    pub fn with_min_batch_size(mut self, n: usize) -> Self {
        let mut cfg = self
            .config
            .batching
            .as_adaptive_config()
            .unwrap_or_default();
        cfg.adaptive_min_batch_size = n;
        self.config.batching = BatchingConfig::adaptive(cfg);
        self
    }

    /// Set the maximum adaptive batch size.
    ///
    /// **Side effect:** calling this enables adaptive mode if the builder
    /// currently has `BatchingConfig::Fixed`.
    pub fn with_max_batch_size(mut self, n: usize) -> Self {
        let mut cfg = self
            .config
            .batching
            .as_adaptive_config()
            .unwrap_or_default();
        cfg.adaptive_max_batch_size = n;
        self.config.batching = BatchingConfig::adaptive(cfg);
        self
    }

    /// Set the adaptive throughput window size (in 100 ms units; `10` = 1 s).
    ///
    /// **Side effect:** calling this enables adaptive mode if the builder
    /// currently has `BatchingConfig::Fixed`.
    pub fn with_window_size(mut self, n: usize) -> Self {
        let mut cfg = self
            .config
            .batching
            .as_adaptive_config()
            .unwrap_or_default();
        cfg.adaptive_window_size = n;
        self.config.batching = BatchingConfig::adaptive(cfg);
        self
    }

    /// Set the adaptive batch flush timeout in milliseconds.
    ///
    /// **Side effect:** calling this enables adaptive mode if the builder
    /// currently has `BatchingConfig::Fixed`.
    pub fn with_batch_timeout_ms(mut self, ms: u64) -> Self {
        let mut cfg = self
            .config
            .batching
            .as_adaptive_config()
            .unwrap_or_default();
        cfg.adaptive_batch_timeout_ms = ms;
        self.config.batching = BatchingConfig::adaptive(cfg);
        self
    }

    pub fn with_output_templates(mut self, templates: OutputTemplates) -> Self {
        self.config.output_templates = Some(templates);
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

    pub fn with_config(mut self, config: GrpcReactionConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> anyhow::Result<GrpcReaction> {
        self.config.validate(&self.queries)?;
        Ok(GrpcReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}

#[cfg(test)]
mod test_server;

#[cfg(test)]
mod tests;

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "grpc-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::GrpcReactionDescriptor],
    bootstrap_descriptors = [],
);
