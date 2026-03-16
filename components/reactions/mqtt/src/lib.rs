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

pub mod config;
mod mqtt;

use config::{
    default_base_topic, default_timeout_ms, default_host, default_port, default_capacity, MqttCallSpec, MqttQueryConfig, MqttReactionConfig,
};
pub use mqtt::MqttReaction;
use std::collections::HashMap;

pub struct MqttReactionBuilder {
    id: String,
    queries: Vec<String>,
    base_topic: String,
    timeout_ms: u64,
    routes: HashMap<String, MqttQueryConfig>,
    host: String,
    port: u16,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    capacity: usize,
}

impl MqttReactionBuilder {
    // Create a new MQTT reaction builder with given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            base_topic: default_base_topic(),
            timeout_ms: default_timeout_ms(),
            routes: HashMap::new(),
            priority_queue_capacity: None,
            auto_start: true,
            host: default_host(),
            port: default_port(),
            capacity: default_capacity(),
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    // Set the query IDs to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set the base topic for MQTT messages
    pub fn with_base_topic(mut self, base_topic: impl Into<String>) -> Self {
        self.base_topic = base_topic.into();
        self
    }

    /// Set the request timeout in milliseconds
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Add a route configuration for a specific query
    pub fn with_route(mut self, query_id: impl Into<String>, config: MqttQueryConfig) -> Self {
        self.routes.insert(query_id.into(), config);
        self
    }

    pub fn with_default_route(mut self, config: MqttQueryConfig) -> Self {
        self.routes.insert("*".to_string(), config);
        self
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: MqttReactionConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.base_topic = config.base_topic;
        self.timeout_ms = config.timeout_ms;
        self.routes = config.routes;
        self
    }

    /// Build the MQTT reaction
    pub fn build(self) -> anyhow::Result<MqttReaction> {
        let config = MqttReactionConfig {
            host: self.host,
            port: self.port,
            base_topic: self.base_topic,
            timeout_ms: self.timeout_ms,
            routes: self.routes,
            capacity: self.capacity
        };

        Ok(MqttReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}
