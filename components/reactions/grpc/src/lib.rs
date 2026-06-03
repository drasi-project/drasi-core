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

//! Unified gRPC reaction plugin for Drasi.

pub(crate) mod adaptive_batcher;
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

pub use config::{BatchingConfig, GrpcReactionConfig, OutputTemplates};
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
        self.with_batching(BatchingConfig::Adaptive(adaptive))
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
