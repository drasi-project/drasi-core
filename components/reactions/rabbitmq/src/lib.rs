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

use std::collections::HashMap;

/// Builder for RabbitMQ reaction
///
/// Creates a RabbitMQReaction instance with a fluent API.
pub struct RabbitMQReactionBuilder {
    id: String,
    queries: Vec<String>,
    connection_string: String,
    exchange_name: String,
    exchange_type: ExchangeType,
    exchange_durable: bool,
    message_persistent: bool,
    tls_enabled: bool,
    tls_cert_path: Option<String>,
    tls_key_path: Option<String>,
    query_configs: HashMap<String, QueryPublishConfig>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl RabbitMQReactionBuilder {
    /// Create a new RabbitMQ reaction builder with the given ID.
    pub fn new(id: impl Into<String>) -> Self {
        let defaults = RabbitMQReactionConfig::default();
        Self {
            id: id.into(),
            queries: Vec::new(),
            connection_string: defaults.connection_string,
            exchange_name: defaults.exchange_name,
            exchange_type: defaults.exchange_type,
            exchange_durable: defaults.exchange_durable,
            message_persistent: defaults.message_persistent,
            tls_enabled: defaults.tls_enabled,
            tls_cert_path: defaults.tls_cert_path,
            tls_key_path: defaults.tls_key_path,
            query_configs: defaults.query_configs,
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
        self.connection_string = connection_string.into();
        self
    }

    /// Set the exchange name and type.
    pub fn with_exchange(
        mut self,
        exchange_name: impl Into<String>,
        exchange_type: ExchangeType,
    ) -> Self {
        self.exchange_name = exchange_name.into();
        self.exchange_type = exchange_type;
        self
    }

    /// Set whether the exchange is durable.
    pub fn with_exchange_durable(mut self, durable: bool) -> Self {
        self.exchange_durable = durable;
        self
    }

    /// Set whether messages are persistent.
    pub fn with_message_persistent(mut self, persistent: bool) -> Self {
        self.message_persistent = persistent;
        self
    }

    /// Enable TLS with optional cert and key paths.
    pub fn with_tls(mut self, cert_path: Option<String>, key_path: Option<String>) -> Self {
        self.tls_enabled = true;
        self.tls_cert_path = cert_path;
        self.tls_key_path = key_path;
        self
    }

    /// Add a publish configuration for a specific query.
    pub fn with_query_config(
        mut self,
        query_id: impl Into<String>,
        config: QueryPublishConfig,
    ) -> Self {
        self.query_configs.insert(query_id.into(), config);
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
        self.connection_string = config.connection_string;
        self.exchange_name = config.exchange_name;
        self.exchange_type = config.exchange_type;
        self.exchange_durable = config.exchange_durable;
        self.message_persistent = config.message_persistent;
        self.tls_enabled = config.tls_enabled;
        self.tls_cert_path = config.tls_cert_path;
        self.tls_key_path = config.tls_key_path;
        self.query_configs = config.query_configs;
        self
    }

    /// Build the RabbitMQ reaction.
    pub fn build(self) -> anyhow::Result<RabbitMQReaction> {
        let config = RabbitMQReactionConfig {
            connection_string: self.connection_string,
            exchange_name: self.exchange_name,
            exchange_type: self.exchange_type,
            exchange_durable: self.exchange_durable,
            message_persistent: self.message_persistent,
            tls_enabled: self.tls_enabled,
            tls_cert_path: self.tls_cert_path,
            tls_key_path: self.tls_key_path,
            query_configs: self.query_configs,
        };

        RabbitMQReaction::from_builder(
            self.id,
            self.queries,
            config,
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
