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

//! Configuration for the RIPE NCC RIS Live source.

use serde::{Deserialize, Serialize};

fn default_websocket_url() -> String {
    "wss://ris-live.ripe.net/v1/ws/".to_string()
}

fn default_include_peer_state() -> bool {
    true
}

fn default_reconnect_delay_secs() -> u64 {
    5
}

fn default_clear_state_on_start() -> bool {
    false
}

/// Initial behavior for stream processing.
///
/// RIS Live does not expose replay-by-offset semantics, but this setting controls
/// how incoming events are filtered when the source starts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum StartFrom {
    /// Process incoming stream events immediately.
    Beginning,
    /// Process incoming stream events immediately.
    #[default]
    Now,
    /// Ignore events whose `timestamp` is older than this Unix timestamp in milliseconds.
    Timestamp { timestamp_ms: i64 },
}

/// RIPE RIS Live source configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RisLiveSourceConfig {
    /// WebSocket endpoint for RIS Live.
    #[serde(default = "default_websocket_url")]
    pub websocket_url: String,
    /// Optional client identifier passed as `?client=` query parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// Optional route collector filter (e.g. `rrc00`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Optional BGP message type filter (e.g. `UPDATE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_type: Option<String>,
    /// Optional prefix filter(s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefixes: Option<Vec<String>>,
    /// Whether to match more specific prefixes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more_specific: Option<bool>,
    /// Whether to match less specific prefixes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub less_specific: Option<bool>,
    /// Optional AS path filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional peer IP filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Optional required field filter (e.g. announcements/withdrawals).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<String>,
    /// Emit peer node updates from `RIS_PEER_STATE` messages.
    #[serde(default = "default_include_peer_state")]
    pub include_peer_state: bool,
    /// Delay before reconnect attempts after disconnects.
    #[serde(default = "default_reconnect_delay_secs")]
    pub reconnect_delay_secs: u64,
    /// Clear persisted graph state at startup.
    #[serde(default = "default_clear_state_on_start")]
    pub clear_state_on_start: bool,
    /// Initial stream behavior.
    #[serde(default)]
    pub start_from: StartFrom,
}

impl Default for RisLiveSourceConfig {
    fn default() -> Self {
        Self {
            websocket_url: default_websocket_url(),
            client_name: None,
            host: None,
            message_type: None,
            prefixes: None,
            more_specific: None,
            less_specific: None,
            path: None,
            peer: None,
            require: None,
            include_peer_state: default_include_peer_state(),
            reconnect_delay_secs: default_reconnect_delay_secs(),
            clear_state_on_start: default_clear_state_on_start(),
            start_from: StartFrom::default(),
        }
    }
}

impl RisLiveSourceConfig {
    /// Returns whether a message with the given timestamp should be processed.
    pub fn should_process_timestamp(&self, message_timestamp_ms: Option<i64>) -> bool {
        match self.start_from {
            StartFrom::Timestamp { timestamp_ms } => match message_timestamp_ms {
                Some(ts) => ts >= timestamp_ms,
                None => true,
            },
            StartFrom::Beginning | StartFrom::Now => true,
        }
    }

    /// Returns a non-zero reconnect delay in seconds.
    pub fn reconnect_delay_secs(&self) -> u64 {
        self.reconnect_delay_secs.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::{RisLiveSourceConfig, StartFrom};

    #[test]
    fn default_values_are_set() {
        let config = RisLiveSourceConfig::default();
        assert_eq!(config.websocket_url, "wss://ris-live.ripe.net/v1/ws/");
        assert!(config.include_peer_state);
        assert_eq!(config.reconnect_delay_secs, 5);
        assert_eq!(config.start_from, StartFrom::Now);
    }

    #[test]
    fn start_from_timestamp_filters_older_messages() {
        let config = RisLiveSourceConfig {
            start_from: StartFrom::Timestamp {
                timestamp_ms: 1_700_000_000_000,
            },
            ..Default::default()
        };

        assert!(!config.should_process_timestamp(Some(1_699_999_999_999)));
        assert!(config.should_process_timestamp(Some(1_700_000_000_001)));
        assert!(config.should_process_timestamp(None));
    }

    #[test]
    fn reconnect_delay_is_never_zero() {
        let config = RisLiveSourceConfig {
            reconnect_delay_secs: 0,
            ..Default::default()
        };
        assert_eq!(config.reconnect_delay_secs(), 1);
    }
}
