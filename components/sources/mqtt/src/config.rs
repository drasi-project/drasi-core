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
use rumqttc::{v5::MqttOptions as MqttOptionsV5, MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::fmt::Debug;

pub fn default_host() -> String {
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

#[derive(Debug, Clone, PartialEq, Serialize_repr, Deserialize_repr, Eq, Default)]
#[repr(u8)]
pub enum MqttQoS {
    ZERO = 0,
    #[default]
    ONE = 1,
    TWO = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttTopicConfig {
    pub topic: String,
    pub qos: MqttQoS,
}

/// Transport mode for MQTT connection.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase", tag = "mode", content = "config")]
pub enum MqttTransportMode {
    #[default]
    #[serde(rename = "tcp")]
    TCP,
    /// TLS transport. Use `ca_path` / `client_cert_path` / `client_key_path` for
    /// file-based configuration, or inline `ca` / `client_auth` bytes directly.
    #[serde(rename = "tls")]
    TLS {
        /// CA certificate bytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca: Option<String>,
        /// Path to CA certificate file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca_path: Option<String>,
        /// ALPN settings.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alpn: Option<Vec<Vec<u8>>>,
        /// TLS client authentication bytes: (cert, key).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_auth: Option<(Vec<u8>, Vec<u8>)>,
        /// Path to client certificate file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_cert_path: Option<String>,
        /// Path to client private key file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_key_path: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MqttTls {
    pub ca: Vec<u8>,
    pub alpn: Option<Vec<Vec<u8>>>,
    pub client_auth: Option<(Vec<u8>, Vec<u8>)>,
}

impl MqttTransportMode {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            MqttTransportMode::TCP => Ok(()),
            MqttTransportMode::TLS {
                ca,
                ca_path,
                alpn,
                client_auth,
                client_cert_path,
                client_key_path,
            } => {
                match (ca, ca_path) {
                    (Some(_), Some(_)) => {
                        return Err(anyhow::anyhow!(
                            "TLS transport: specify either 'ca' or 'ca_path', not both"
                        ));
                    }
                    (None, None) => {
                        return Err(anyhow::anyhow!(
                            "TLS transport requires either 'ca' bytes or 'ca_path'"
                        ));
                    }
                    _ => {}
                }

                let has_inline_client_auth = client_auth.is_some();
                let has_path_client_auth = client_cert_path.is_some() || client_key_path.is_some();

                if has_inline_client_auth && has_path_client_auth {
                    return Err(anyhow::anyhow!(
                        "TLS transport: specify either 'client_auth' or ('client_cert_path' and 'client_key_path'), not both"
                    ));
                }

                if client_cert_path.is_some() ^ client_key_path.is_some() {
                    return Err(anyhow::anyhow!(
                        "TLS transport: 'client_cert_path' and 'client_key_path' must be set together"
                    ));
                }

                Ok(())
            }
        }
    }

    pub fn get_tls_config(&self) -> anyhow::Result<MqttTls> {
        self.validate()?;

        match self {
            MqttTransportMode::TCP => {
                Err(anyhow::anyhow!("TCP transport does not have TLS config"))
            }
            MqttTransportMode::TLS {
                ca,
                ca_path,
                alpn,
                client_auth,
                client_cert_path,
                client_key_path,
            } => {
                let ca = match (ca, ca_path) {
                    (Some(ca), None) => ca.clone().into_bytes(),
                    (None, Some(path)) => Self::read_file(path, "ca_path")?,
                    _ => unreachable!("validated above"),
                };

                let client_auth = match (client_auth, client_cert_path, client_key_path) {
                    (Some(auth), None, None) => Some(auth.clone()),
                    (None, Some(cert_path), Some(key_path)) => Some((
                        Self::read_file(cert_path, "client_cert_path")?,
                        Self::read_file(key_path, "client_key_path")?,
                    )),
                    (None, None, None) => None,
                    _ => unreachable!("validated above"),
                };

                Ok(MqttTls {
                    ca,
                    alpn: alpn.clone(),
                    client_auth,
                })
            }
        }
    }

    fn read_file(path: &str, field: &str) -> anyhow::Result<Vec<u8>> {
        fs::read(path).map_err(|e| anyhow::anyhow!("Failed to read TLS {field} at '{path}': {e}"))
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct MappingEntity {
    pub label: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MappingMode {
    PayloadAsField,
    PayloadSpread,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct MappingProperties {
    pub mode: MappingMode,
    pub field_name: Option<String>,
    /// JSON object mapping topic variables to graph properties
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inject: Vec<HashMap<String, String>>,
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
    pub id: String,    // relation id template
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
        rumqttc::v5::mqttbytes::v5::ConnectProperties {
            session_expiry_interval: self.session_expiry_interval,
            receive_maximum: self.receive_maximum,
            max_packet_size: self.max_packet_size,
            topic_alias_max: self.topic_alias_max,
            request_response_info: self.request_response_info,
            request_problem_info: self.request_problem_info,
            user_properties: self.user_properties.clone(),
            authentication_method: self.authentication_method.clone(),
            authentication_data: self.authentication_data.clone().map(Bytes::from),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttSubscribeProperties {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<(String, String)>,
}

impl MqttSubscribeProperties {
    pub fn to_subscribe_properties(&self) -> rumqttc::v5::mqttbytes::v5::SubscribeProperties {
        rumqttc::v5::mqttbytes::v5::SubscribeProperties {
            id: self.id,
            user_properties: self.user_properties.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MqttSourceConfig {
    //...... Main config, mqtt-wide config parameters
    /// MQTT broker host address
    #[serde(default = "default_host")]
    pub host: String,

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

    //...... Authentication config parameters
    /// Username for MQTT authentication (if not using identity provider)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,

    /// Password for MQTT authentication (if not using identity provider)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl MqttSourceConfig {
    /// Validate the configuration and return an error if invalid.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.port == 0 {
            return Err(anyhow::anyhow!("Port number cannot be 0"));
        }
        if let Some(timeout) = self.conn_timeout {
            if timeout == 0 {
                return Err(anyhow::anyhow!("Connection timeout cannot be 0"));
            }
        }
        if let (Some(min_batch), Some(max_batch)) =
            (self.adaptive_min_batch_size, self.adaptive_max_batch_size)
        {
            if min_batch > max_batch {
                return Err(anyhow::anyhow!(
                    "Adaptive batching: min batch size cannot exceed max batch size"
                ));
            }
        }
        if let (Some(min_wait), Some(max_wait)) =
            (self.adaptive_min_wait_ms, self.adaptive_max_wait_ms)
        {
            if min_wait > max_wait {
                return Err(anyhow::anyhow!(
                    "Adaptive batching: min wait time cannot exceed max wait time"
                ));
            }
        }

        if self.max_incoming_packet_size.is_none() ^ self.max_outgoing_packet_size.is_none() {
            return Err(anyhow::anyhow!(
                "Both max_incoming_packet_size and max_outgoing_packet_size must be set together"
            ));
        }

        for topic_mapping in &self.topic_mappings {
            if topic_mapping.properties.mode == MappingMode::PayloadAsField
                && topic_mapping.properties.field_name.is_none()
            {
                return Err(anyhow::anyhow!(
                    "field_name must be set for PayloadAsField mode in topic mapping with pattern: {}",
                    topic_mapping.pattern
                ));
            }

            if topic_mapping.properties.mode == MappingMode::PayloadSpread
                && topic_mapping.properties.field_name.is_some()
            {
                return Err(anyhow::anyhow!(
                    "field_name must not be set for PayloadSpread mode in topic mapping with pattern: {}",
                    topic_mapping.pattern
                ));
            }

            Self::validate_mapping_placeholders(topic_mapping)?;
        }

        if self.username.is_some() ^ self.password.is_some() {
            return Err(anyhow::anyhow!(
                "Both username and password must be set together for authentication"
            ));
        }

        if let Some(transport) = &self.transport {
            transport.validate()?;
        }

        Ok(())
    }

    pub fn max_packet_sizes(&self) -> Option<(usize, usize)> {
        if self.max_incoming_packet_size.is_some() && self.max_outgoing_packet_size.is_some() {
            let incoming = self.max_incoming_packet_size?;
            let outgoing = self.max_outgoing_packet_size?;
            Some((incoming, outgoing))
        } else {
            None
        }
    }

    fn validate_mapping_placeholders(mapping: &TopicMapping) -> anyhow::Result<()> {
        let mut router = matchit::Router::new();
        router.insert(&mapping.pattern, ()).map_err(|e| {
            anyhow::anyhow!("Invalid topic mapping pattern '{}': {}", mapping.pattern, e)
        })?;

        let allowed = Self::extract_placeholders(&mapping.pattern).map_err(|e| {
            anyhow::anyhow!("Invalid topic mapping pattern '{}': {}", mapping.pattern, e)
        })?;

        Self::validate_template_placeholders(
            &mapping.entity.id,
            &allowed,
            "entity.id",
            &mapping.pattern,
        )?;

        if let Some(field_name) = mapping.properties.field_name.as_ref() {
            Self::validate_template_placeholders(
                field_name,
                &allowed,
                "properties.field_name",
                &mapping.pattern,
            )?;
        }

        for inject in &mapping.properties.inject {
            for (key, value) in inject {
                Self::validate_template_placeholders(
                    value,
                    &allowed,
                    &format!("properties.inject['{key}']"),
                    &mapping.pattern,
                )?;
            }
        }

        for (idx, node) in mapping.nodes.iter().enumerate() {
            Self::validate_template_placeholders(
                &node.id,
                &allowed,
                &format!("nodes[{idx}].id"),
                &mapping.pattern,
            )?;
        }

        for (idx, relation) in mapping.relations.iter().enumerate() {
            Self::validate_template_placeholders(
                &relation.id,
                &allowed,
                &format!("relations[{idx}].id"),
                &mapping.pattern,
            )?;
        }

        Ok(())
    }

    fn validate_template_placeholders(
        template: &str,
        allowed: &HashSet<String>,
        field_name: &str,
        pattern: &str,
    ) -> anyhow::Result<()> {
        let placeholders = Self::extract_placeholders(template).map_err(|e| {
            anyhow::anyhow!(
                "Invalid placeholder syntax in {field_name} for pattern '{pattern}': {e}",
            )
        })?;

        for placeholder in placeholders {
            if !allowed.contains(&placeholder) {
                return Err(anyhow::anyhow!(
                    "Unknown placeholder '{{{placeholder}}}' in {field_name} for pattern '{pattern}'. Allowed placeholders come from the pattern."
                ));
            }
        }

        Ok(())
    }

    fn extract_placeholders(input: &str) -> anyhow::Result<HashSet<String>> {
        let mut result = HashSet::new();
        let mut current = String::new();
        let mut in_placeholder = false;

        for c in input.chars() {
            match c {
                '{' => {
                    if in_placeholder {
                        return Err(anyhow::anyhow!("nested '{{' is not allowed"));
                    }
                    in_placeholder = true;
                    current.clear();
                }
                '}' => {
                    if !in_placeholder {
                        return Err(anyhow::anyhow!("found '}}' without matching '{{'"));
                    }
                    if current.is_empty() {
                        return Err(anyhow::anyhow!("empty placeholder '{{}}' is not allowed"));
                    }
                    result.insert(current.clone());
                    in_placeholder = false;
                }
                _ => {
                    if in_placeholder {
                        current.push(c);
                    }
                }
            }
        }

        if in_placeholder {
            return Err(anyhow::anyhow!("found '{{' without matching '}}'"));
        }

        Ok(result)
    }
}

impl Default for MqttSourceConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            identity_provider: None,
            topics: vec![],
            topic_mappings: vec![],
            event_channel_capacity: default_event_channel_capacity(),
            transport: None,
            request_channel_capacity: None,
            max_inflight: None,
            keep_alive: None,
            clean_start: None,
            max_incoming_packet_size: None,
            max_outgoing_packet_size: None,
            conn_timeout: Some(5000),
            connect_properties: None,
            subscribe_properties: None,
            adaptive_max_batch_size: None,
            adaptive_min_batch_size: None,
            adaptive_max_wait_ms: None,
            adaptive_min_wait_ms: None,
            adaptive_window_secs: None,
            adaptive_enabled: None,
            username: None,
            password: None,
        }
    }
}

impl Debug for MqttSourceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttSourceConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("topics", &self.topics)
            .field("topic_mappings", &self.topic_mappings)
            .field("event_channel_capacity", &self.event_channel_capacity)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::from_str;

    fn topic_mapping(mode: MappingMode, field_name: Option<&str>) -> TopicMapping {
        TopicMapping {
            pattern: "building/{floor}/{room}/{device}".to_string(),
            entity: MappingEntity {
                label: "DEVICE".to_string(),
                id: "{room}:{device}".to_string(),
            },
            properties: MappingProperties {
                mode,
                field_name: field_name.map(str::to_string),
                inject: vec![HashMap::from([("room".to_string(), "{room}".to_string())])],
            },
            nodes: vec![MappingNode {
                label: "FLOOR".to_string(),
                id: "{floor}".to_string(),
            }],
            relations: vec![MappingRelation {
                label: "CONTAINS".to_string(),
                from: "FLOOR".to_string(),
                to: "ROOM".to_string(),
                id: "{floor}_contains_{room}".to_string(),
            }],
        }
    }

    fn valid_config() -> MqttSourceConfig {
        MqttSourceConfig {
            host: "localhost".to_string(),
            port: 1883,
            identity_provider: None,
            topics: vec![MqttTopicConfig {
                topic: "sensors/temperature".to_string(),
                qos: MqttQoS::ONE,
            }],
            topic_mappings: vec![topic_mapping(MappingMode::PayloadAsField, Some("reading"))],
            event_channel_capacity: 20,
            transport: None,
            request_channel_capacity: Some(10),
            max_inflight: Some(100),
            keep_alive: Some(30),
            clean_start: Some(true),
            max_incoming_packet_size: None,
            max_outgoing_packet_size: None,
            conn_timeout: Some(5000),
            connect_properties: None,
            subscribe_properties: Some(MqttSubscribeProperties {
                id: Some(7),
                user_properties: vec![("source".to_string(), "mqtt".to_string())],
            }),
            adaptive_max_batch_size: Some(32),
            adaptive_min_batch_size: Some(8),
            adaptive_max_wait_ms: Some(1000),
            adaptive_min_wait_ms: Some(100),
            adaptive_window_secs: Some(5),
            adaptive_enabled: Some(true),
            username: None,
            password: None,
        }
    }

    #[test]
    fn default_config_has_expected_values() {
        let config = MqttSourceConfig::default();

        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 1883);
        assert_eq!(config.event_channel_capacity, 20);
        assert_eq!(config.conn_timeout, Some(5000));
        assert_eq!(default_qos(), MqttQoS::ONE);
        assert_eq!(MqttQoS::default(), MqttQoS::ONE);
        assert!(config.topics.is_empty());
        assert!(config.topic_mappings.is_empty());
        assert_eq!(config.max_packet_sizes(), None);
    }

    #[test]
    fn valid_config_passes_validation() {
        let config = valid_config();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_invalid_port() {
        let config = MqttSourceConfig {
            port: 0,
            ..Default::default()
        };

        let error = config.validate().expect_err("port 0 should be invalid");
        assert!(error.to_string().contains("Port number cannot be 0"));
    }

    #[test]
    fn validate_rejects_invalid_timeout() {
        let config = MqttSourceConfig {
            conn_timeout: Some(0),
            ..Default::default()
        };

        let error = config.validate().expect_err("timeout 0 should be invalid");
        assert!(error.to_string().contains("Connection timeout cannot be 0"));
    }

    #[test]
    fn validate_rejects_invalid_adaptive_batching_bounds() {
        let config = MqttSourceConfig {
            adaptive_min_batch_size: Some(5),
            adaptive_max_batch_size: Some(3),
            ..Default::default()
        };

        let error = config
            .validate()
            .expect_err("min batch above max batch should be invalid");
        assert!(error
            .to_string()
            .contains("min batch size cannot exceed max batch size"));

        let config = MqttSourceConfig {
            adaptive_min_wait_ms: Some(500),
            adaptive_max_wait_ms: Some(100),
            ..Default::default()
        };

        let error = config
            .validate()
            .expect_err("min wait above max wait should be invalid");
        assert!(error
            .to_string()
            .contains("min wait time cannot exceed max wait time"));
    }

    #[test]
    fn validate_rejects_unpaired_packet_sizes() {
        let mut config = valid_config();
        config.max_incoming_packet_size = Some(1024);

        let error = config
            .validate()
            .expect_err("only one packet size should be invalid");
        assert!(error.to_string().contains("must be set together"));

        let mut config = valid_config();
        config.max_outgoing_packet_size = Some(2048);

        let error = config
            .validate()
            .expect_err("only one packet size should be invalid");
        assert!(error.to_string().contains("must be set together"));
    }

    #[test]
    fn max_packet_sizes_returns_pair_when_both_are_present() {
        let mut config = valid_config();
        config.max_incoming_packet_size = Some(1024);
        config.max_outgoing_packet_size = Some(2048);

        assert_eq!(config.validate().ok(), Some(()));
        assert_eq!(config.max_packet_sizes(), Some((1024, 2048)));
    }

    #[test]
    fn validate_rejects_payload_as_field_without_field_name() {
        let mut config = valid_config();
        config.topic_mappings = vec![topic_mapping(MappingMode::PayloadAsField, None)];

        let error = config
            .validate()
            .expect_err("payload_as_field requires a field name");
        assert!(error.to_string().contains("field_name must be set"));
    }

    #[test]
    fn validate_rejects_payload_spread_with_field_name() {
        let mut config = valid_config();
        config.topic_mappings = vec![topic_mapping(MappingMode::PayloadSpread, Some("reading"))];

        let error = config
            .validate()
            .expect_err("payload_spread must not define field_name");
        assert!(error.to_string().contains("field_name must not be set"));
    }

    #[test]
    fn validate_accepts_payload_spread_without_field_name() {
        let mut config = valid_config();
        config.topic_mappings = vec![topic_mapping(MappingMode::PayloadSpread, None)];

        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_unknown_placeholder_in_templates() {
        let mut config = valid_config();
        config.topic_mappings[0].entity.id = "{unknown}:{device}".to_string();

        let error = config
            .validate()
            .expect_err("unknown placeholders should be rejected");

        assert!(error
            .to_string()
            .contains("Unknown placeholder '{unknown}'"));
    }

    #[test]
    fn validate_rejects_invalid_placeholder_syntax_in_pattern() {
        let mut config = valid_config();
        config.topic_mappings[0].pattern = "building/{floor/{room}/{device}".to_string();

        let error = config
            .validate()
            .expect_err("invalid pattern placeholder syntax should be rejected");

        assert!(error.to_string().contains("Invalid topic mapping pattern"));
    }

    #[test]
    fn validate_rejects_invalid_placeholder_syntax_in_template() {
        let mut config = valid_config();
        config.topic_mappings[0].entity.id = "{room:{device}".to_string();

        let error = config
            .validate()
            .expect_err("invalid template placeholder syntax should be rejected");

        assert!(error
            .to_string()
            .contains("Invalid placeholder syntax in entity.id"));
    }

    #[test]
    fn validate_rejects_empty_placeholder_in_template() {
        let mut config = valid_config();
        config.topic_mappings[0].entity.id = "{}:{device}".to_string();

        let error = config
            .validate()
            .expect_err("empty placeholders should be rejected");

        assert!(error.to_string().contains("empty placeholder"));
    }

    #[test]
    fn yaml_deserializes_full_config_schema() {
        let yaml = r#"
host: "mqtt.example.com"
port: 9001
topics:
  - topic: "sensors/temperature"
    qos: 1
topic_mappings:
  - pattern: "building/{floor}/{room}/{device}"
    entity:
      label: "DEVICE"
      id: "{room}:{device}"
    properties:
      mode: payload_as_field
      field_name: "reading"
      inject:
        - room: "{room}"
        - floor: "{floor}"
    nodes:
      - label: "FLOOR"
        id: "{floor}"
      - label: "ROOM"
        id: "{room}"
    relations:
      - label: "CONTAINS"
        from: "FLOOR"
        to: "ROOM"
        id: "{floor}_contains_{room}"
      - label: "LOCATED_IN"
        from: "ROOM"
        to: "DEVICE"
        id: "{device}_located_in_{room}"
event_channel_capacity: 64
adaptive_enabled: true
adaptive_min_batch_size: 2
adaptive_max_batch_size: 8
adaptive_min_wait_ms: 50
adaptive_max_wait_ms: 500
adaptive_window_secs: 3
"#;

        let config: MqttSourceConfig = from_str(yaml).expect("yaml should deserialize");

        assert_eq!(config.host, "mqtt.example.com");
        assert_eq!(config.port, 9001);
        assert_eq!(config.topics.len(), 1);
        assert_eq!(config.topics[0].qos, MqttQoS::ONE);
        assert_eq!(config.topic_mappings.len(), 1);
        assert_eq!(
            config.topic_mappings[0].properties.mode,
            MappingMode::PayloadAsField
        );
        assert_eq!(
            config.topic_mappings[0].properties.field_name.as_deref(),
            Some("reading")
        );
        assert_eq!(config.topic_mappings[0].properties.inject.len(), 2);
        assert_eq!(config.event_channel_capacity, 64);
        assert_eq!(config.adaptive_enabled, Some(true));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn yaml_deserializes_payload_spread_without_field_name() {
        let yaml = r#"
topic_mappings:
  - pattern: "sensors/{type}"
    entity:
      label: "READING"
      id: "{type}"
    properties:
      mode: payload_spread
"#;

        let config: MqttSourceConfig = from_str(yaml).expect("yaml should deserialize");

        assert_eq!(config.host, "localhost");
        assert_eq!(
            config.topic_mappings[0].properties.mode,
            MappingMode::PayloadSpread
        );
        assert_eq!(config.topic_mappings[0].properties.field_name, None);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn connect_properties_conversion_preserves_values() {
        let config = MqttConnectProperties {
            session_expiry_interval: Some(30),
            receive_maximum: Some(25),
            max_packet_size: Some(4096),
            topic_alias_max: Some(8),
            request_response_info: Some(1),
            request_problem_info: Some(0),
            user_properties: vec![("env".to_string(), "test".to_string())],
            authentication_method: Some("token".to_string()),
            authentication_data: Some(vec![1, 2, 3]),
        };

        let props = config.to_connection_properties();

        assert_eq!(props.session_expiry_interval, Some(30));
        assert_eq!(props.receive_maximum, Some(25));
        assert_eq!(props.max_packet_size, Some(4096));
        assert_eq!(props.topic_alias_max, Some(8));
        assert_eq!(props.request_response_info, Some(1));
        assert_eq!(props.request_problem_info, Some(0));
        assert_eq!(
            props.user_properties,
            vec![("env".to_string(), "test".to_string())]
        );
        assert_eq!(props.authentication_method.as_deref(), Some("token"));
        assert_eq!(props.authentication_data, Some(Bytes::from(vec![1, 2, 3])));
    }

    #[test]
    fn rejecting_conflicting_tls_sources() {
        let mut config = valid_config();
        config.transport = Some(MqttTransportMode::TLS {
            ca: Some("ca-bytes".to_string()),
            ca_path: Some("/tmp/ca.pem".to_string()),
            alpn: None,
            client_auth: None,
            client_cert_path: None,
            client_key_path: None,
        });

        let err = config
            .validate()
            .expect_err("ca and ca_path together should be invalid");
        assert!(err.to_string().contains("specify either 'ca' or 'ca_path'"));
    }

    #[test]
    fn yaml_deserializes_tls_transport_with_paths() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();

        let base = std::env::temp_dir();
        let ca_path = base.join(format!("drasi-mqtt-{unique}-ca.pem"));
        let cert_path = base.join(format!("drasi-mqtt-{unique}-client.crt"));
        let key_path = base.join(format!("drasi-mqtt-{unique}-client.key"));

        fs::write(&ca_path, b"ca-bytes").expect("should write CA file");
        fs::write(&cert_path, b"cert-bytes").expect("should write cert file");
        fs::write(&key_path, b"key-bytes").expect("should write key file");

        let yaml = format!(
            r#"
transport:
    mode: tls
    config:
        ca_path: "{}"
        client_cert_path: "{}"
        client_key_path: "{}"
"#,
            ca_path.display(),
            cert_path.display(),
            key_path.display(),
        );

        let config: MqttSourceConfig = from_str(&yaml).expect("yaml should deserialize");
        assert!(config.validate().is_ok());

        let transport = config.transport.expect("transport should be set");
        match transport {
            MqttTransportMode::TLS {
                ca,
                ca_path,
                alpn,
                client_auth,
                client_cert_path,
                client_key_path,
            } => {
                assert!(ca.is_none());
                assert!(ca_path.is_some());
                assert!(client_auth.is_none());
                assert_eq!(
                    client_cert_path.as_deref(),
                    Some(cert_path.to_string_lossy().as_ref())
                );
                assert_eq!(
                    client_key_path.as_deref(),
                    Some(key_path.to_string_lossy().as_ref())
                );
            }
            MqttTransportMode::TCP => panic!("expected TLS transport"),
        }

        let transport = MqttTransportMode::TLS {
            ca: None,
            ca_path: Some(ca_path.to_string_lossy().to_string()),
            alpn: None,
            client_auth: None,
            client_cert_path: Some(cert_path.to_string_lossy().to_string()),
            client_key_path: Some(key_path.to_string_lossy().to_string()),
        };

        let resolved = transport.get_tls_config().expect("resolution should work");
        assert_eq!(resolved.ca, b"ca-bytes".to_vec());
        assert_eq!(
            resolved.client_auth,
            Some((b"cert-bytes".to_vec(), b"key-bytes".to_vec()))
        );

        // remove the temp files.
        fs::remove_file(ca_path).expect("cleanup should remove ca file");
        fs::remove_file(cert_path).expect("cleanup should remove cert file");
        fs::remove_file(key_path).expect("cleanup should remove key file");
    }

    #[test]
    fn yaml_deserialization_tcp_transport() {
        let yaml = r#"
transport:
    mode: tcp
"#;

        let config: MqttSourceConfig = from_str(yaml).expect("yaml should deserialize");
        assert!(config.validate().is_ok());

        let transport = config.transport.expect("transport should be set");
        match transport {
            MqttTransportMode::TCP => {}
            MqttTransportMode::TLS { .. } => panic!("expected TCP transport"),
        }
    }

    #[test]
    fn yaml_deserialization_rejects_conflicting_tls_fields() {
        let yaml = r#"
transport:
    mode: tls
    config:
        ca: "ca-bytes"
        ca_path: "/tmp/ca.pem"
        client_cert_path: "/tmp/client.crt"
        client_key_path: "/tmp/client.key"
"#;
        let config: MqttSourceConfig = from_str(yaml).expect("yaml should deserialize");
        let err = config
            .validate()
            .expect_err("conflicting TLS fields should be rejected");
        assert!(err.to_string().contains("specify either 'ca' or 'ca_path'"));
    }

    #[test]
    fn yaml_username_without_password_is_invalid() {
        let yaml = r#"
username: "mqtt_user"
"#;
        let config: MqttSourceConfig = from_str(yaml).expect("yaml should deserialize");
        let err = config
            .validate()
            .expect_err("username without password should be invalid");
        assert!(err.to_string().contains("Both username and password must be set together"));
    }

    #[test]
    fn yaml_password_without_username_is_invalid() {
        let yaml = r#"
password: "mqtt_pass"
"#;
        let config: MqttSourceConfig = from_str(yaml).expect("yaml should deserialize");
        let err = config
            .validate()
            .expect_err("password without username should be invalid");
        assert!(err.to_string().contains("Both username and password must be set together"));   
    }

    #[test]
    fn yaml_wrong_input() {
        let yaml = r#"
host: "mqtt.example.com"
port: "not a number"
"#;
        let err= serde_yaml::from_str::<MqttSourceConfig>(yaml).expect_err("yaml with wrong types should fail to deserialize");
        assert!(err.to_string().contains("invalid type: string \"not a number\", expected u16 for at line 3 column 7"));
    }

    #[test]
    fn yaml_wrong_tag() {
        let yaml = r#"
host: "mqtt.example.com"
port: 1883
transport:
    mode: unknown
"#;
        let err = serde_yaml::from_str::<MqttSourceConfig>(yaml).expect_err("yaml with unknown transport mode should fail to deserialize");
        assert!(err.to_string().contains("unknown variant `unknown`, expected `tcp` or `tls` at line 5 column 11"));
    }
}