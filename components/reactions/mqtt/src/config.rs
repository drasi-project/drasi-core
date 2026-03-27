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

use drasi_lib::reactions::common::AdaptiveBatchConfig;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, default};

pub fn default_topic() -> String {
    "".to_string()
}

pub fn default_keep_alive() -> u64 {
    60 // seconds
}

pub fn default_connection_timeout() -> u64 {
    30 // seconds
}

pub fn default_clean_session() -> bool {
    true
}

pub fn default_port() -> u16 {
    1883
}

pub fn default_broker_addr() -> String {
    "localhost".to_string()
}

pub fn default_request_channel_capacity() -> usize {
    10 // following rumqttc default
}

pub fn default_event_channel_capacity() -> usize {
    20
}

pub fn default_max_packet_size() -> u32 {
    10 * 1024 // following rumqttc default
}

pub fn default_pending_throttle() -> u64 {
    0 // following rumqttc default
}

pub fn default_inflight() -> u16 {
    100 // following rumqttc defaults
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

/// Authentication configuration for MQTT broker connection.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub enum MqttAuthMode {
    /// No authentication (default)
    #[default]
    None,

    /// Username and password authentication
    UsernamePassword { username: String, password: String },
}

/// Transport mode for MQTT connection.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub enum MqttTransportMode {
    #[default]
    TCP,
    TLS {
        /// ca certificate
        ca: Vec<u8>,
        /// alpn settings
        alpn: Option<Vec<Vec<u8>>>,
        /// tls client_authentication
        client_auth: Option<(Vec<u8>, Vec<u8>)>,
    },
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
    #[serde(default = "default_broker_addr")]
    pub broker_addr: String,

    /// MQTT broker port (default: 1883)
    #[serde(default = "default_port")]
    pub port: u16,

    /// What transport mode to use when connecting to the MQTT broker (default: TCP)
    #[serde(default)]
    pub transport_mode: MqttTransportMode,

    /// Request timeout in milliseconds
    #[serde(default = "default_keep_alive")]
    pub keep_alive: u64,

    /// Clean or Persistent session for MQTT connection (default: true)
    #[serde(default = "default_clean_session")]
    pub clean_session: bool,

    /// Authentication mode for connecting to the MQTT broker.
    #[serde(default)]
    pub auth_mode: MqttAuthMode,

    /// Request (publish, subscribe) channel capacity
    #[serde(default = "default_request_channel_capacity")]
    pub request_channel_capacity: usize,

    /// Capacity of the async channel
    #[serde(default = "default_event_channel_capacity")]
    pub event_channel_capacity: usize,

    /// Minimum detlay time between consecutive outgoing packets
    /// While retransmitting pending packets.
    #[serde(default = "default_pending_throttle")]
    pub pending_throttle: u64,

    /// Connection timeout
    #[serde(default = "default_connection_timeout")]
    pub connection_timeout: u64,

    /// Default value of for maximum incoming packet size.
    /// Used when `max_incomming_size` in `connect_properties` is NOT available.
    #[serde(default = "default_max_packet_size")]
    pub max_packet_size: u32,

    /// Connection properties since we follow V5
    /// TODO

    /// Maximum number of outgoing inflight messages
    #[serde(default = "default_inflight")]
    pub max_inflight: u16,

    /// Base topic for MQTT requests
    #[serde(default = "default_topic")]
    pub default_topic: String,

    /// Query-specific call configurations
    #[serde(default)]
    pub query_configs: HashMap<String, MqttQueryConfig>,

    /// Adaptive batching configuration (flattened into parent config)
    #[serde(flatten)]
    pub adaptive: Option<AdaptiveBatchConfig>,
}
impl Default for MqttReactionConfig {
    fn default() -> Self {
        Self {
            broker_addr: default_broker_addr(),
            port: default_port(),
            transport_mode: MqttTransportMode::default(),
            keep_alive: default_keep_alive(),
            clean_session: default_clean_session(),
            auth_mode: MqttAuthMode::default(),
            request_channel_capacity: default_request_channel_capacity(),
            event_channel_capacity: default_event_channel_capacity(),
            pending_throttle: default_pending_throttle(),
            connection_timeout: default_connection_timeout(),
            max_packet_size: default_max_packet_size(),
            max_inflight: default_inflight(),
            default_topic: default_topic(),
            query_configs: HashMap::new(),
            adaptive: None, // default to no adaptive batching
        }
    }
}
