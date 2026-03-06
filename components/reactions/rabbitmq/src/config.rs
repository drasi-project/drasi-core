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

//! Configuration types for RabbitMQ reactions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_connection_string() -> String {
    "amqp://guest:guest@localhost:5672/%2f".to_string()
}

fn default_exchange_name() -> String {
    "drasi-events".to_string()
}

fn default_exchange_durable() -> bool {
    true
}

fn default_message_persistent() -> bool {
    true
}

/// Exchange type mapping for RabbitMQ.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExchangeType {
    Direct,
    Topic,
    Fanout,
    Headers,
}

impl Default for ExchangeType {
    fn default() -> Self {
        Self::Topic
    }
}

impl ExchangeType {
    pub fn as_exchange_kind(&self) -> lapin::ExchangeKind {
        match self {
            ExchangeType::Direct => lapin::ExchangeKind::Direct,
            ExchangeType::Topic => lapin::ExchangeKind::Topic,
            ExchangeType::Fanout => lapin::ExchangeKind::Fanout,
            ExchangeType::Headers => lapin::ExchangeKind::Headers,
        }
    }
}

/// Publish specification for a single operation type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PublishSpec {
    /// Routing key (supports Handlebars templates).
    pub routing_key: String,
    /// Custom AMQP headers (values support Handlebars templates).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Optional Handlebars template for message body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_template: Option<String>,
}

/// Query-specific publish configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct QueryPublishConfig {
    /// Publish specification for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<PublishSpec>,
    /// Publish specification for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<PublishSpec>,
    /// Publish specification for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<PublishSpec>,
}

/// RabbitMQ reaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RabbitMQReactionConfig {
    /// AMQP connection string.
    #[serde(default = "default_connection_string")]
    pub connection_string: String,
    /// Exchange name for publishing.
    #[serde(default = "default_exchange_name")]
    pub exchange_name: String,
    /// Exchange type.
    #[serde(default)]
    pub exchange_type: ExchangeType,
    /// Whether the exchange is durable.
    #[serde(default = "default_exchange_durable")]
    pub exchange_durable: bool,
    /// Whether messages are published as persistent.
    #[serde(default = "default_message_persistent")]
    pub message_persistent: bool,
    /// Enable TLS for AMQPS connections.
    #[serde(default)]
    pub tls_enabled: bool,
    /// Optional PEM certificate chain for TLS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
    /// Optional PKCS#12 client identity (PFX).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<String>,
    /// Query-specific publish configuration.
    #[serde(default)]
    pub query_configs: HashMap<String, QueryPublishConfig>,
}

impl Default for RabbitMQReactionConfig {
    fn default() -> Self {
        Self {
            connection_string: default_connection_string(),
            exchange_name: default_exchange_name(),
            exchange_type: ExchangeType::default(),
            exchange_durable: default_exchange_durable(),
            message_persistent: default_message_persistent(),
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
            query_configs: HashMap::new(),
        }
    }
}

impl RabbitMQReactionConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.connection_string.trim().is_empty() {
            return Err(anyhow::anyhow!("connection_string must not be empty"));
        }
        if self.exchange_name.trim().is_empty() {
            return Err(anyhow::anyhow!("exchange_name must not be empty"));
        }
        if self.tls_enabled && !self.connection_string.starts_with("amqps://") {
            return Err(anyhow::anyhow!(
                "tls_enabled requires an amqps:// connection_string"
            ));
        }
        if !self.tls_enabled && (self.tls_cert_path.is_some() || self.tls_key_path.is_some()) {
            return Err(anyhow::anyhow!(
                "tls_cert_path/tls_key_path require tls_enabled = true"
            ));
        }
        Ok(())
    }
}
