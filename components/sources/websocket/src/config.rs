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

//! Configuration for the WebSocket source.
//!
//! # Example
//!
//! ```yaml
//! url: "wss://feed.example.com/events"
//! itemsPath: events
//! mappings:
//!   - operation: insert
//!     elementType: node
//!     template:
//!       id: "{{payload.id}}"
//!       labels: ["Sensor"]
//! ```

use std::{collections::HashSet, fmt};

use anyhow::{ensure, Context, Result};
use drasi_source_mapping::SourceMapping;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::http::header::{HeaderName, HeaderValue};
use url::Url;

const MAX_URL_LENGTH: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_INITIAL_MESSAGES: usize = 32;
const MAX_MAPPINGS: usize = 64;
const MIN_MESSAGE_SIZE: usize = 1024;
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
const MAX_BUFFER_CAPACITY: usize = 1024;
const DEFAULT_MAX_RECONNECT_DELAY_MS: u64 = 30_000;

const MANAGED_HEADERS: &[&str] = &[
    "host",
    "connection",
    "upgrade",
    "content-length",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
];

/// One HTTP header added to the WebSocket upgrade request.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeaderConfig {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

impl fmt::Debug for HeaderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeaderConfig")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// Retry behavior for connection failures and disconnects.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReconnectConfig {
    /// Whether retryable failures and disconnects are retried. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Initial exponential-backoff delay in milliseconds. Default: 1,000;
    /// valid range: 100–300,000.
    #[serde(default = "default_reconnect_delay_ms")]
    pub delay_ms: u64,
    /// Maximum exponential reconnect delay. When omitted, the effective maximum
    /// is at least 30 seconds and never less than `delay_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delay_ms: Option<u64>,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            delay_ms: default_reconnect_delay_ms(),
            max_delay_ms: None,
        }
    }
}

impl ReconnectConfig {
    pub(crate) fn effective_max_delay_ms(&self) -> u64 {
        self.max_delay_ms
            .unwrap_or_else(|| self.delay_ms.max(DEFAULT_MAX_RECONNECT_DELAY_MS))
    }
}

/// Runtime configuration for [`crate::WebSocketSource`].
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSocketSourceConfig {
    /// WebSocket endpoint. Must be an absolute `wss://` URL, or `ws://` when
    /// `allow_insecure` is enabled.
    pub url: String,
    /// Allows cleartext `ws://` connections. Default: `false`.
    #[serde(default)]
    pub allow_insecure: bool,
    /// Headers added to the upgrade request. Default: empty; maximum: 64.
    #[serde(default)]
    pub headers: Vec<HeaderConfig>,
    /// Timeout for each connection attempt in milliseconds. Default: 10,000;
    /// valid range: 100–300,000.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// JSON text messages sent after every successful handshake. Default:
    /// empty; maximum: 32, each bounded by `max_message_size_bytes`.
    #[serde(default)]
    pub initial_messages: Vec<String>,
    /// Reconnect behavior. Default: enabled with a 1,000 ms initial delay.
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    /// `$` for the whole frame, or one top-level array field. Default: `$`.
    #[serde(default = "default_items_path")]
    pub items_path: String,
    /// Payload mappings that produce graph changes. Required; 1–64 mappings.
    pub mappings: Vec<SourceMapping>,
    /// Maximum accepted WebSocket message and frame size. Default: 1 MiB;
    /// valid range: 1 KiB–16 MiB.
    #[serde(default = "default_max_message_size_bytes")]
    pub max_message_size_bytes: usize,
    /// Capacity of each query subscriber channel. Default: 64; valid range:
    /// 1–1,024.
    #[serde(default = "default_buffer_capacity")]
    pub buffer_capacity: usize,
}

impl fmt::Debug for WebSocketSourceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocketSourceConfig")
            .field("url", &"<redacted>")
            .field("allow_insecure", &self.allow_insecure)
            .field("headers", &self.headers)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("initial_message_count", &self.initial_messages.len())
            .field("reconnect", &self.reconnect)
            .field("items_path", &self.items_path)
            .field("mapping_count", &self.mappings.len())
            .field("max_message_size_bytes", &self.max_message_size_bytes)
            .field("buffer_capacity", &self.buffer_capacity)
            .finish()
    }
}

impl Default for WebSocketSourceConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            allow_insecure: false,
            headers: Vec::new(),
            connect_timeout_ms: default_connect_timeout_ms(),
            initial_messages: Vec::new(),
            reconnect: ReconnectConfig::default(),
            items_path: default_items_path(),
            mappings: Vec::new(),
            max_message_size_bytes: default_max_message_size_bytes(),
            buffer_capacity: default_buffer_capacity(),
        }
    }
}

impl WebSocketSourceConfig {
    /// Validates the complete source configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when any configured value or mapping is invalid.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.url.is_empty() && self.url.len() <= MAX_URL_LENGTH,
            "url must be between 1 and {MAX_URL_LENGTH} bytes"
        );

        let url = Url::parse(&self.url).context("url must be a valid WebSocket URL")?;
        ensure!(url.host_str().is_some(), "url must include a host");
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "url must not include user information"
        );
        ensure!(url.fragment().is_none(), "url must not include a fragment");

        match url.scheme() {
            "wss" => {}
            "ws" => ensure!(
                self.allow_insecure,
                "allowInsecure must be true for ws:// endpoints"
            ),
            _ => anyhow::bail!("url scheme must be ws or wss"),
        }

        ensure!(
            (100..=300_000).contains(&self.connect_timeout_ms),
            "connectTimeoutMs must be between 100 and 300000"
        );
        ensure!(
            self.headers.len() <= MAX_HEADERS,
            "headers cannot contain more than {MAX_HEADERS} entries"
        );

        let mut header_names = HashSet::new();
        for header in &self.headers {
            let normalized = header.name.to_ascii_lowercase();
            ensure!(
                !MANAGED_HEADERS.contains(&normalized.as_str()),
                "header '{}' is managed by the WebSocket client",
                header.name
            );
            ensure!(
                header_names.insert(normalized),
                "duplicate header '{}'",
                header.name
            );
            HeaderName::from_bytes(header.name.as_bytes())
                .with_context(|| format!("invalid header name '{}'", header.name))?;
            HeaderValue::from_str(&header.value)
                .with_context(|| format!("invalid value for header '{}'", header.name))?;
        }

        ensure!(
            (MIN_MESSAGE_SIZE..=MAX_MESSAGE_SIZE).contains(&self.max_message_size_bytes),
            "maxMessageSizeBytes must be between {MIN_MESSAGE_SIZE} and {MAX_MESSAGE_SIZE}"
        );
        ensure!(
            (1..=MAX_BUFFER_CAPACITY).contains(&self.buffer_capacity),
            "bufferCapacity must be between 1 and {MAX_BUFFER_CAPACITY}"
        );
        ensure!(
            self.initial_messages.len() <= MAX_INITIAL_MESSAGES,
            "initialMessages cannot contain more than {MAX_INITIAL_MESSAGES} entries"
        );
        for (index, message) in self.initial_messages.iter().enumerate() {
            ensure!(
                message.len() <= self.max_message_size_bytes,
                "initialMessages[{index}] exceeds maxMessageSizeBytes"
            );
            serde_json::from_str::<serde_json::Value>(message)
                .with_context(|| format!("initialMessages[{index}] must contain valid JSON"))?;
        }

        ensure!(
            (100..=300_000).contains(&self.reconnect.delay_ms),
            "reconnect.delayMs must be between 100 and 300000"
        );
        if let Some(max_delay_ms) = self.reconnect.max_delay_ms {
            ensure!(
                (100..=300_000).contains(&max_delay_ms),
                "reconnect.maxDelayMs must be between 100 and 300000"
            );
            ensure!(
                self.reconnect.delay_ms <= max_delay_ms,
                "reconnect.delayMs must not exceed reconnect.maxDelayMs"
            );
        }

        validate_items_path(&self.items_path)?;
        ensure!(!self.mappings.is_empty(), "mappings cannot be empty");
        ensure!(
            self.mappings.len() <= MAX_MAPPINGS,
            "mappings cannot contain more than {MAX_MAPPINGS} entries"
        );
        for (index, mapping) in self.mappings.iter().enumerate() {
            mapping
                .validate()
                .with_context(|| format!("mappings[{index}] is invalid"))?;
            if let Some(condition) = &mapping.when {
                ensure!(
                    condition.header.is_none(),
                    "mappings[{index}].when.header is not supported by WebSocket sources"
                );
                ensure!(
                    condition
                        .field
                        .as_deref()
                        .is_some_and(|field| !field.trim().is_empty()),
                    "mappings[{index}].when.field must be specified"
                );
                let comparator_count = [
                    condition.equals.is_some(),
                    condition.contains.is_some(),
                    condition.regex.is_some(),
                ]
                .into_iter()
                .filter(|present| *present)
                .count();
                ensure!(
                    comparator_count == 1,
                    "mappings[{index}].when must specify exactly one of equals, contains, or regex"
                );

                if let Some(pattern) = condition.regex.as_deref() {
                    ensure!(
                        regex::Regex::new(pattern).is_ok(),
                        "mappings[{index}].when.regex must be a valid regular expression"
                    );
                }
            }
        }

        Ok(())
    }
}

fn validate_items_path(path: &str) -> Result<()> {
    if path == "$" {
        return Ok(());
    }
    ensure!(!path.is_empty(), "itemsPath cannot be empty");
    ensure!(
        !path.starts_with('$') && !path.contains('.'),
        "itemsPath must be '$' or one top-level field name"
    );
    Ok(())
}

const fn default_true() -> bool {
    true
}

const fn default_connect_timeout_ms() -> u64 {
    10_000
}

const fn default_reconnect_delay_ms() -> u64 {
    1_000
}

fn default_items_path() -> String {
    "$".to_string()
}

const fn default_max_message_size_bytes() -> usize {
    1024 * 1024
}

const fn default_buffer_capacity() -> usize {
    64
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use drasi_source_mapping::{
        ElementTemplate, ElementType, MappingCondition, OperationType, SourceMapping,
    };

    use super::*;

    fn mapping() -> SourceMapping {
        SourceMapping {
            when: None,
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Node,
            effective_from: None,
            template: ElementTemplate {
                id: "{{payload.id}}".to_string(),
                labels: vec!["Item".to_string()],
                properties: None,
                from: None,
                to: None,
            },
        }
    }

    fn valid_config() -> WebSocketSourceConfig {
        WebSocketSourceConfig {
            url: "wss://example.com/events".to_string(),
            mappings: vec![mapping()],
            ..Default::default()
        }
    }

    #[test]
    fn default_values_match_runtime_configuration_defaults() {
        let config = WebSocketSourceConfig::default();

        assert!(!config.allow_insecure);
        assert!(config.headers.is_empty());
        assert_eq!(config.connect_timeout_ms, 10_000);
        assert!(config.initial_messages.is_empty());
        assert!(config.reconnect.enabled);
        assert_eq!(config.reconnect.delay_ms, 1_000);
        assert_eq!(config.reconnect.max_delay_ms, None);
        assert_eq!(
            config.reconnect.effective_max_delay_ms(),
            DEFAULT_MAX_RECONNECT_DELAY_MS
        );
        assert_eq!(config.items_path, "$");
        assert!(config.mappings.is_empty());
        assert_eq!(config.max_message_size_bytes, 1024 * 1024);
        assert_eq!(config.buffer_capacity, 64);
    }

    #[test]
    fn accepts_minimal_secure_config() {
        valid_config().validate().unwrap();
    }

    #[test]
    fn requires_explicit_insecure_opt_in() {
        let mut config = valid_config();
        config.url = "ws://localhost:8080/events".to_string();
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "allowInsecure must be true for ws:// endpoints"
        );

        config.allow_insecure = true;
        config.validate().unwrap();
    }

    #[test]
    fn url_validation_errors_do_not_expose_resolved_values() {
        let mut config = valid_config();
        config.url = "private-scheme://example.com/events".to_string();

        let error = config.validate().unwrap_err().to_string();
        assert_eq!(error, "url scheme must be ws or wss");
        assert!(!error.contains("private-scheme"));
    }

    #[test]
    fn rejects_websocket_managed_headers() {
        let mut config = valid_config();
        config.headers.push(HeaderConfig {
            name: "Sec-WebSocket-Protocol".to_string(),
            value: "custom".to_string(),
        });

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("Sec-WebSocket-Protocol"), "{error}");
        assert!(error.contains("managed by the WebSocket client"), "{error}");
    }

    #[test]
    fn rejects_non_json_initial_messages() {
        let mut config = valid_config();
        config.initial_messages.push("{invalid".to_string());

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("initialMessages[0]"), "{error}");
        assert!(error.contains("valid JSON"), "{error}");
    }

    #[test]
    fn rejects_http_header_mapping_conditions() {
        let mut config = valid_config();
        config.mappings[0].when = Some(MappingCondition {
            header: Some("X-Test".to_string()),
            field: None,
            equals: Some("value".to_string()),
            contains: None,
            regex: None,
        });
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "mappings[0].when.header is not supported by WebSocket sources"
        );
    }

    #[test]
    fn rejects_invalid_mapping_regexes() {
        let mut config = valid_config();
        config.mappings[0].when = Some(MappingCondition {
            header: None,
            field: Some("payload.kind".to_string()),
            equals: None,
            contains: None,
            regex: Some("(".to_string()),
        });

        let error = config.validate().unwrap_err().to_string();
        assert_eq!(
            error,
            "mappings[0].when.regex must be a valid regular expression"
        );
    }

    #[test]
    fn rejects_incomplete_or_ambiguous_mapping_conditions() {
        let cases = [
            (
                MappingCondition {
                    header: None,
                    field: None,
                    equals: Some("sensor".to_string()),
                    contains: None,
                    regex: None,
                },
                "mappings[0].when.field must be specified",
            ),
            (
                MappingCondition {
                    header: None,
                    field: Some("payload.kind".to_string()),
                    equals: None,
                    contains: None,
                    regex: None,
                },
                "mappings[0].when must specify exactly one of equals, contains, or regex",
            ),
            (
                MappingCondition {
                    header: None,
                    field: Some("payload.kind".to_string()),
                    equals: Some("sensor".to_string()),
                    contains: Some("sens".to_string()),
                    regex: None,
                },
                "mappings[0].when must specify exactly one of equals, contains, or regex",
            ),
        ];

        for (condition, expected_error) in cases {
            let mut config = valid_config();
            config.mappings[0].when = Some(condition);
            assert_eq!(config.validate().unwrap_err().to_string(), expected_error);
        }
    }

    #[test]
    fn accepts_dynamic_operation_mapping_configuration() {
        let mut config = valid_config();
        config.mappings[0].operation = None;
        config.mappings[0].operation_from = Some("payload.op".to_string());
        config.mappings[0].operation_map = Some(HashMap::from([(
            "insert".to_string(),
            OperationType::Insert,
        )]));
        config.validate().unwrap();
    }

    #[test]
    fn rejects_nested_items_paths() {
        let mut config = valid_config();
        config.items_path = "data.events".to_string();
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "itemsPath must be '$' or one top-level field name"
        );
    }

    #[test]
    fn omitted_maximum_reconnect_delay_uses_the_greater_default_or_initial_delay() {
        let mut config = valid_config();
        config.reconnect.delay_ms = 60_000;

        config.validate().unwrap();
        assert_eq!(config.reconnect.effective_max_delay_ms(), 60_000);

        config.reconnect.delay_ms = 1_000;
        assert_eq!(
            config.reconnect.effective_max_delay_ms(),
            DEFAULT_MAX_RECONNECT_DELAY_MS
        );
    }

    #[test]
    fn rejects_maximum_reconnect_delay_below_initial_delay() {
        let mut config = valid_config();
        config.reconnect.delay_ms = 2_000;
        config.reconnect.max_delay_ms = Some(1_000);

        let error = config.validate().unwrap_err().to_string();
        assert_eq!(
            error,
            "reconnect.delayMs must not exceed reconnect.maxDelayMs"
        );
    }

    #[test]
    fn debug_redacts_sensitive_configuration() {
        let mut config = valid_config();
        config.url =
            "wss://user-secret:password-secret@example-secret.com/path-secret?token=query-secret#fragment-secret"
                .to_string();
        config.headers.push(HeaderConfig {
            name: "Authorization".to_string(),
            value: "header-secret".to_string(),
        });
        config
            .initial_messages
            .push(r#"{"token":"message-secret"}"#.to_string());

        let debug = format!("{config:?}");
        for secret in [
            "user-secret",
            "password-secret",
            "example-secret.com",
            "path-secret",
            "query-secret",
            "fragment-secret",
            "header-secret",
            "message-secret",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("<redacted>"));
    }
}
