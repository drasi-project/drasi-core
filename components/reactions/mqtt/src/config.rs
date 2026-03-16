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

//! Configuration types for MQTT reactions.
//!
//! This module contains configuration types for MQTT reaction and shared types.

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, default};

pub fn default_base_topic() -> String {
    "".to_string()
}

pub fn default_timeout_ms() -> u64 {
    5000
}

pub fn default_port() -> u16 {
    1883
}

pub fn default_host() -> String {
    "localhost".to_string()
}

pub fn default_capacity() -> usize {
    10
}

/// MQTT message retain policy.
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RetainPolicy {
    Retain,
    #[default]
    NoRetain,
}

/// QoS level for MQTT messages.
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QualityOfService {
    AtMostOnce,
    AtLeastOnce,
    #[default]
    ExactlyOnce,
}

/// Specification for an MQTT call, including topic, retain policy, headers, and body template.
///
/// This type is used to configure MQTT requests for different operation types (added, updated, deleted).
/// All fields support Handlebars template syntax for dynamic content generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttCallSpec {
    /// MQTT topic (appended to base_url) or absolute topic.
    /// Supports Handlebars templates for dynamic topics.
    pub topic: String,

    /// Request body as a Handlebars template.
    /// If empty, sends the raw JSON data.
    #[serde(default)]
    pub body: String,

    /// MQTT message retain policy.
    #[serde(default)]
    pub retain: RetainPolicy,

    /// Additional MQTT v5 headers as key-value pairs.
    /// Header values support Handlebars templates.
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// QoS level for MQTT messages.
    #[serde(default)]
    pub qos: QualityOfService,
}

/// Configuration for query-specific MQTT calls.
///
/// Defines different MQTT call specifications for each operation type (added, updated, deleted).
/// Each operation type can have its own topic, retain policy, body template, and headers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttQueryConfig {
    /// MQTT call specification for ADD operations (new rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<MqttCallSpec>,

    /// MQTT call specification for UPDATE operations (modified rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<MqttCallSpec>,

    /// MQTT call specification for DELETE operations (removed rows from query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<MqttCallSpec>,
}

/// MQTT reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttReactionConfig {

    /// MQTT broker host (default: "localhost")
    #[serde(default = "default_host")]
    pub host: String,

    /// MQTT broker port (default: 1883)
    #[serde(default = "default_port")]
    pub port: u16,

    /// Base topic for MQTT requests
    #[serde(default = "default_base_topic")]
    pub base_topic: String,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Query-specific call configurations
    #[serde(default)]
    pub routes: HashMap<String, MqttQueryConfig>,

    /// Capacity of the priority queue for incoming query results
    #[serde(default)]
    pub capacity: usize,
}

impl Default for MqttReactionConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            base_topic: default_base_topic(),
            timeout_ms: default_timeout_ms(),
            routes: HashMap::new(),
            capacity: default_capacity(),
        }
    }
}
