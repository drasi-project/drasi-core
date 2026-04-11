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

//! Configuration types for MQTT source.
//!
//! This module contains configuration types for MQTT source and shared types.
use bytes::Bytes;
use drasi_lib::identity::IdentityProvider;
use rumqttc::v5::MqttOptions as MqttOptionsV5;
use rumqttc::{MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub fn default_broker_addr() -> String {
    "localhost".to_string()
}

pub fn default_port() -> u16 {
    1883
}

pub fn default_qos() -> MqttQoS {
    MqttQoS::ONE
}

pub fn default_event_channel_capacity() -> usize {
    20
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq)]
pub enum MqttQoS {
    ZERO,
    ONE,
    TWO,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttTopicConfig {
    pub topic: String,
    pub qos: MqttQoS,
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingEntity {
    pub label: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingProperties {
    pub mode: String,
    pub field_name: Option<String>,
    pub inject: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingNode {
    pub label: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingRelation {
    pub label: String, // relation label
    pub from: String,  // label if the source node
    pub to: String,    // label of the destination node
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopicMapping {
    pub pattern: String,
    pub entity: MappingEntity,
    pub properties: MappingProperties,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<MappingNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<MappingRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttConnectProperties {
    /// Expiry interval property after loosing connection
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_expiry_interval: Option<u32>,
    /// Maximum simultaneous packets
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_maximum: Option<u16>,
    /// Maximum packet size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_packet_size: Option<u32>,
    /// Maximum mapping integer for a topic
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_alias_max: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_response_info: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_problem_info: Option<u8>,
    /// List of user properties
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<(String, String)>,
    /// Method of authentication
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication_method: Option<String>,
    /// Authentication data
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication_data: Option<Vec<u8>>,
}

impl MqttConnectProperties {
    pub fn to_connection_properties(&self) -> rumqttc::v5::mqttbytes::v5::ConnectProperties {
        let mut props = rumqttc::v5::mqttbytes::v5::ConnectProperties {
            session_expiry_interval: self.session_expiry_interval,
            receive_maximum: self.receive_maximum,
            max_packet_size: self.max_packet_size,
            topic_alias_max: self.topic_alias_max,
            request_response_info: self.request_response_info,
            request_problem_info: self.request_problem_info,
            user_properties: self.user_properties.clone(),
            authentication_method: self.authentication_method.clone(),
            authentication_data: self.authentication_data.clone().map(Bytes::from),
        };
        props
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttSubscribeProperties {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<(String, String)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MQTTSourceConfig {
    //...... Main config, mqtt-wide config parameters
    /// MQTT broker host address
    #[serde(default = "default_broker_addr")]
    pub broker_addr: String,

    /// MQTT broker port
    #[serde(default = "default_port")]
    pub port: u16,

    /// Identity provider for authentication (takes precedence over user/password)
    #[serde(skip)]
    pub identity_provider: Option<Box<dyn IdentityProvider>>,

    /// MQTT topic to subscribe to
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<MqttTopicConfig>,

    /// MQTT Topic mapping configuration: maps incoming MQTT topic hierarchy to Drasi entities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topic_mappings: Vec<TopicMapping>,

    /// Capacity of the async channel
    #[serde(default = "default_event_channel_capacity")] // for client creation and event loop
    pub event_channel_capacity: usize,

    /// Quality of Service level for MQTT messages // for subscribe
    #[serde(default = "default_qos")]
    pub qos: MqttQoS,

    //...... Shared config parameters (used by both v3 and v5 clients)
    /// MQTT transport protocol (e.g., "tcp", "tls")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<MqttTransportMode>,

    /// Request (publish, subscribe) channel capacity
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_channel_capacity: Option<usize>,

    /// Maximum number of outgoing inflight messages
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight: Option<u16>,

    /// Keep alive interval in Seconds (PingReq)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<u64>,

    /// Clean or Persistent session for MQTT connection (default: true)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean_start: Option<bool>,

    //...... MQTT v3 specific config parameters (if any)
    /// Max incoming packet size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_incoming_packet_size: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_outgoing_packet_size: Option<usize>,

    //...... MQTT v5 specific config parameters (if any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn_timeout: Option<u64>, // Connection timeout in milliseconds for MQTT v5

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_properties: Option<MqttConnectProperties>, // MQTT v5 connect properties

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe_properties: Option<MqttSubscribeProperties>, // MQTT v5 subscribe properties

    //...... Adaptive batching config parameters
    /// Adaptive batching: maximum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_batch_size: Option<usize>,

    /// Adaptive batching: minimum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_batch_size: Option<usize>,

    /// Adaptive batching: maximum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_wait_ms: Option<u64>,

    /// Adaptive batching: minimum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_wait_ms: Option<u64>,

    /// Adaptive batching: throughput window in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_window_secs: Option<u64>,

    /// Whether adaptive batching is enabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_enabled: Option<bool>,
}
