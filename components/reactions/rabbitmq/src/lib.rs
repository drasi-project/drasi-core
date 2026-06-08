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

//! RabbitMQ reaction plugin for Drasi

pub mod config;
pub mod descriptor;
pub mod rabbitmq;

#[cfg(test)]
mod tests;

pub use config::{ExchangeType, PublishSpec, QueryPublishConfig, RabbitMQReactionConfig};
pub use rabbitmq::RabbitMQReaction;

/// Builder for RabbitMQ reaction
///
/// Creates a RabbitMQReaction instance with a fluent API.
pub struct RabbitMQReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: RabbitMQReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl RabbitMQReactionBuilder {
    /// Create a new RabbitMQ reaction builder with the given ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: RabbitMQReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the query IDs to subscribe to.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to.
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set the AMQP connection string.
    pub fn with_connection_string(mut self, connection_string: impl Into<String>) -> Self {
        self.config.connection_string = connection_string.into();
        self
    }

    /// Set the exchange name and type.
    pub fn with_exchange(
        mut self,
        exchange_name: impl Into<String>,
        exchange_type: ExchangeType,
    ) -> Self {
        self.config.exchange_name = exchange_name.into();
        self.config.exchange_type = exchange_type;
        self
    }

    /// Set whether the exchange is durable.
    pub fn with_exchange_durable(mut self, durable: bool) -> Self {
        self.config.exchange_durable = durable;
        self
    }

    /// Set whether messages are persistent.
    pub fn with_message_persistent(mut self, persistent: bool) -> Self {
        self.config.message_persistent = persistent;
        self
    }

    /// Enable TLS with optional cert and PFX identity paths.
    pub fn with_tls(mut self, cert_path: Option<String>, pfx_path: Option<String>) -> Self {
        self.config.tls_enabled = true;
        self.config.tls_cert_path = cert_path;
        self.config.tls_pfx_path = pfx_path;
        self
    }

    /// Set the TLS PFX password.
    pub fn with_tls_pfx_password(mut self, password: impl Into<String>) -> Self {
        self.config.tls_pfx_password = Some(password.into());
        self
    }

    /// Add a publish configuration for a specific query.
    pub fn with_query_config(
        mut self,
        query_id: impl Into<String>,
        config: QueryPublishConfig,
    ) -> Self {
        self.config.query_configs.insert(query_id.into(), config);
        self
    }

    /// Set the maximum number of reconnect attempts.
    pub fn with_max_reconnect_attempts(mut self, attempts: u32) -> Self {
        self.config.max_reconnect_attempts = attempts;
        self
    }

    /// Set the priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once.
    pub fn with_config(mut self, config: RabbitMQReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Build the RabbitMQ reaction.
    pub fn build(self) -> anyhow::Result<RabbitMQReaction> {
        RabbitMQReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "rabbitmq-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::RabbitMQReactionDescriptor],
    bootstrap_descriptors = [],
);
