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

//! RIPE RIS Live protocol message types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::RisLiveSourceConfig;

/// Wrapper for incoming server messages.
#[derive(Debug, Clone, Deserialize)]
pub struct RisIncomingMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: Option<Value>,
}

/// Generic RIS error payload.
#[derive(Debug, Clone, Deserialize)]
pub struct RisErrorData {
    pub message: String,
}

/// BGP announcement structure for UPDATE messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Announcement {
    pub next_hop: String,
    pub prefixes: Vec<String>,
}

/// AS path segment can be either an ASN or an AS_SET.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AsPathSegment {
    Asn(i64),
    AsSet(Vec<i64>),
}

/// Parsed `ris_message` payload data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RisMessageData {
    pub timestamp: Option<f64>,
    pub peer: Option<String>,
    pub peer_asn: Option<String>,
    pub id: Option<String>,
    pub host: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub path: Option<Vec<AsPathSegment>>,
    pub origin: Option<String>,
    pub community: Option<Vec<Vec<i64>>>,
    pub announcements: Option<Vec<Announcement>>,
    pub withdrawals: Option<Vec<String>>,
    pub state: Option<String>,
}

/// Prefix filter in subscribe payload can be a single string or list.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum PrefixFilter {
    One(String),
    Many(Vec<String>),
}

/// Socket options in subscribe payload.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SocketOptions {
    pub acknowledge: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_raw: Option<bool>,
}

/// Data payload for `ris_subscribe`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RisSubscribeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<PrefixFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more_specific: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub less_specific: Option<bool>,
    #[serde(rename = "socketOptions")]
    pub socket_options: SocketOptions,
}

/// Message sent by client to subscribe to RIS Live.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RisSubscribeMessage {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub data: RisSubscribeData,
}

impl RisSubscribeMessage {
    /// Build a `ris_subscribe` message from source config.
    pub fn from_config(config: &RisLiveSourceConfig) -> Self {
        let prefix = config.prefixes.as_ref().and_then(|prefixes| {
            if prefixes.is_empty() {
                None
            } else if prefixes.len() == 1 {
                Some(PrefixFilter::One(prefixes[0].clone()))
            } else {
                Some(PrefixFilter::Many(prefixes.clone()))
            }
        });

        Self {
            msg_type: "ris_subscribe",
            data: RisSubscribeData {
                host: config.host.clone(),
                msg_type: config.message_type.clone(),
                require: config.require.clone(),
                peer: config.peer.clone(),
                path: config.path.clone(),
                prefix,
                more_specific: config.more_specific,
                less_specific: config.less_specific,
                socket_options: SocketOptions {
                    acknowledge: true,
                    include_raw: Some(false),
                },
            },
        }
    }
}

/// Convert message timestamp in seconds to milliseconds.
pub fn message_timestamp_millis(message: &RisMessageData) -> Option<i64> {
    message
        .timestamp
        .map(|seconds| (seconds * 1000.0).round())
        .and_then(|ms| i64::try_from(ms as i128).ok())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::RisLiveSourceConfig;

    use super::{
        message_timestamp_millis, AsPathSegment, RisIncomingMessage, RisMessageData,
        RisSubscribeMessage,
    };

    #[test]
    fn deserialize_update_message_with_announcements() {
        let payload = json!({
            "type": "ris_message",
            "data": {
                "timestamp": 1773098494.83,
                "peer": "208.80.153.193",
                "peer_asn": "14907",
                "id": "msg-1",
                "host": "rrc00.ripe.net",
                "type": "UPDATE",
                "path": [14907, 3356, [64512, 64513]],
                "origin": "INCOMPLETE",
                "community": [[3356, 5]],
                "announcements": [
                    {"next_hop": "208.80.153.193", "prefixes": ["38.190.126.0/24"]}
                ],
                "withdrawals": []
            }
        });

        let incoming: RisIncomingMessage =
            serde_json::from_value(payload).expect("valid incoming payload");
        assert_eq!(incoming.msg_type, "ris_message");

        let msg: RisMessageData = serde_json::from_value(incoming.data.expect("message data"))
            .expect("valid message data");
        assert_eq!(msg.msg_type.as_deref(), Some("UPDATE"));
        assert_eq!(msg.peer.as_deref(), Some("208.80.153.193"));
        assert_eq!(msg.announcements.as_ref().map(Vec::len), Some(1));

        let path = msg.path.expect("path should be present");
        assert_eq!(path[0], AsPathSegment::Asn(14907));
        assert_eq!(path[2], AsPathSegment::AsSet(vec![64512, 64513]));
    }

    #[test]
    fn deserialize_withdraw_only_update() {
        let payload = json!({
            "timestamp": 1773098494.83,
            "peer": "208.80.153.193",
            "peer_asn": "14907",
            "id": "msg-2",
            "host": "rrc00.ripe.net",
            "type": "UPDATE",
            "withdrawals": ["203.0.113.0/24"]
        });

        let msg: RisMessageData = serde_json::from_value(payload).expect("valid message data");
        assert!(msg.announcements.is_none());
        assert_eq!(msg.withdrawals, Some(vec!["203.0.113.0/24".to_string()]));
    }

    #[test]
    fn deserialize_peer_state_message() {
        let payload = json!({
            "timestamp": 1773098494.83,
            "peer": "208.80.153.193",
            "peer_asn": "14907",
            "id": "msg-3",
            "host": "rrc00.ripe.net",
            "type": "RIS_PEER_STATE",
            "state": "down"
        });

        let msg: RisMessageData = serde_json::from_value(payload).expect("valid message data");
        assert_eq!(msg.msg_type.as_deref(), Some("RIS_PEER_STATE"));
        assert_eq!(msg.state.as_deref(), Some("down"));
    }

    #[test]
    fn build_subscribe_message_from_config() {
        let config = RisLiveSourceConfig {
            host: Some("rrc00".to_string()),
            message_type: Some("UPDATE".to_string()),
            prefixes: Some(vec!["203.0.113.0/24".to_string()]),
            path: Some("3356".to_string()),
            ..Default::default()
        };

        let message = RisSubscribeMessage::from_config(&config);
        let as_json = serde_json::to_value(&message).expect("subscribe should serialize");

        assert_eq!(as_json["type"], "ris_subscribe");
        assert_eq!(as_json["data"]["host"], "rrc00");
        assert_eq!(as_json["data"]["type"], "UPDATE");
        assert_eq!(as_json["data"]["path"], "3356");
        assert_eq!(as_json["data"]["prefix"], "203.0.113.0/24");
        assert_eq!(as_json["data"]["socketOptions"]["acknowledge"], true);
    }

    #[test]
    fn timestamp_conversion_to_millis() {
        let message = RisMessageData {
            timestamp: Some(1773098494.83),
            peer: None,
            peer_asn: None,
            id: None,
            host: None,
            msg_type: None,
            path: None,
            origin: None,
            community: None,
            announcements: None,
            withdrawals: None,
            state: None,
        };

        assert_eq!(message_timestamp_millis(&message), Some(1_773_098_494_830));
    }
}
