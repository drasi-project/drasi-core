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

//! Type definitions for Platform Reaction based on Drasi Platform TypeSpec schema

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use drasi_core::evaluation::context::{QueryPartEvaluationContext, QueryVariables};

// /// Result event that can be either a Change or Control event
// #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
// #[serde(tag = "kind", rename_all = "camelCase")]
// pub enum ResultEvent {
//     /// Data change event containing query results
//     Change(ChangeEvent),
//     /// Control event for lifecycle signals
//     Control(ControlEvent),
// }

// /// Change event containing added, updated, and deleted results
// #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
// #[serde(rename_all = "camelCase")]
// pub struct ChangeEvent {
//     /// Query identifier
//     pub query_id: String,

//     /// Sequence number for ordering
//     pub sequence: i64,

//     /// Source timestamp in milliseconds since epoch
//     pub source_time_ms: i64,

//     /// Results that were added (always serialized, even if empty)
//     pub added_results: Vec<serde_json::Value>,

//     /// Results that were updated (with before/after states, always serialized)
//     pub updated_results: Vec<UpdatePayload>,

//     /// Results that were deleted (always serialized, even if empty)
//     pub deleted_results: Vec<serde_json::Value>,

//     /// Optional metadata
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub metadata: Option<HashMap<String, serde_json::Value>>,
// }

// /// Update payload containing before and after states
// #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
// #[serde(rename_all = "camelCase")]
// pub struct UpdatePayload {
//     /// State before the update
//     pub before: serde_json::Value,

//     /// State after the update
//     pub after: serde_json::Value,

//     /// Optional grouping keys for the result
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub grouping_keys: Option<Vec<String>>,
// }

// /// Control event for lifecycle signals
// #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
// #[serde(rename_all = "camelCase")]
// pub struct ControlEvent {
//     /// The control signal type
//     pub control_signal: ControlSignal,
// }

// /// Control signal types for query lifecycle
// #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
// #[serde(rename_all = "camelCase")]
// pub enum ControlSignal {
//     /// Bootstrap process has started
//     BootstrapStarted,

//     /// Bootstrap process has completed
//     BootstrapCompleted,

//     /// Query is running normally
//     Running,

//     /// Query has been stopped
//     Stopped,

//     /// Query has been deleted
//     Deleted,
// }

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind")]
pub enum ControlSignal {
    #[serde(rename = "bootstrapStarted")]
    BootstrapStarted,

    #[serde(rename = "bootstrapCompleted")]
    BootstrapCompleted,

    #[serde(rename = "running")]
    Running,

    #[serde(rename = "stopped")]
    Stopped,

    #[serde(rename = "deleted")]
    QueryDeleted,
}

/// Represents an outgoing result event

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum ResultEvent {
    #[serde(rename = "change")]
    Change(ResultChangeEvent),

    #[serde(rename = "control")]
    Control(ResultControlEvent),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ResultChangeEvent {
    pub query_id: String,
    pub sequence: u64,
    pub source_time_ms: u64,
    pub added_results: Vec<Map<String, Value>>,
    pub updated_results: Vec<UpdatePayload>,
    pub deleted_results: Vec<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ResultControlEvent {
    pub query_id: String,
    pub sequence: u64,
    pub source_time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    pub control_signal: ControlSignal,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouping_keys: Option<Vec<String>>,
}

impl ResultEvent {
    pub fn from_query_results(
        query_id: &str,
        data: Vec<QueryPartEvaluationContext>,
        sequence: u64,
        source_time_ms: u64,
        metadata: Option<Map<String, Value>>,
    ) -> Self {
        let mut added_results = Vec::new();
        let mut updated_results = Vec::new();
        let mut deleted_results = Vec::new();

        for ctx in data {
            match ctx {
                QueryPartEvaluationContext::Adding { after } => {
                    added_results.push(variables_to_json(after));
                }
                QueryPartEvaluationContext::Updating { before, after } => {
                    updated_results.push(UpdatePayload {
                        before: Some(variables_to_json(before)),
                        after: Some(variables_to_json(after)),
                        grouping_keys: None,
                    });
                }
                QueryPartEvaluationContext::Removing { before } => {
                    deleted_results.push(variables_to_json(before));
                }
                QueryPartEvaluationContext::Aggregation {
                    before,
                    after,
                    grouping_keys,
                    ..
                } => {
                    updated_results.push(UpdatePayload {
                        before: before.map(variables_to_json),
                        after: Some(variables_to_json(after)),
                        grouping_keys: Some(grouping_keys),
                    });
                }
                QueryPartEvaluationContext::Noop => {}
            }
        }

        ResultEvent::Change(ResultChangeEvent {
            query_id: query_id.to_string(),
            sequence,
            source_time_ms,
            added_results,
            updated_results,
            deleted_results,
            metadata,
        })
    }

    pub fn from_control_signal(
        query_id: &str,
        sequence: u64,
        source_time_ms: u64,
        control_signal: ControlSignal,
    ) -> Self {
        ResultEvent::Control(ResultControlEvent {
            query_id: query_id.to_string(),
            sequence,
            source_time_ms,
            metadata: None,
            control_signal,
        })
    }
}

/// CloudEvent envelope wrapping ResultEvent data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudEvent<T> {
    /// The event payload
    pub data: T,

    /// Content type (always "application/json")
    pub datacontenttype: String,

    /// Unique event identifier (UUID v4)
    pub id: String,

    /// Pub/sub name for Dapr
    pub pubsubname: String,

    /// Event source identifier
    pub source: String,

    /// CloudEvents spec version (always "1.0")
    pub specversion: String,

    /// Event timestamp (ISO 8601)
    pub time: String,

    /// Topic/stream name
    pub topic: String,

    /// Optional trace ID for observability
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub traceid: String,

    /// Optional W3C trace parent
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub traceparent: String,

    /// Optional trace state
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub tracestate: String,

    /// Event type (always "com.dapr.event.sent")
    #[serde(rename = "type")]
    pub event_type: String,
}

/// Configuration for CloudEvent generation
#[derive(Debug, Clone)]
pub struct CloudEventConfig {
    /// Pub/sub name
    pub pubsub_name: String,

    /// Source identifier
    pub source: String,
}

impl CloudEventConfig {
    /// Create new config with defaults
    pub fn new() -> Self {
        Self {
            pubsub_name: "drasi-pubsub".to_string(),
            source: "drasi-core".to_string(),
        }
    }

    /// Create config with custom values
    pub fn with_values(pubsub_name: String, source: String) -> Self {
        Self {
            pubsub_name,
            source,
        }
    }
}

impl Default for CloudEventConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> CloudEvent<T>
where
    T: Serialize,
{
    /// Create a new CloudEvent
    pub fn new(data: T, query_id: &str, config: &CloudEventConfig) -> Self {
        let now = chrono::Utc::now();
        let id = uuid::Uuid::new_v4().to_string();
        let topic = format!("{query_id}-results");

        Self {
            data,
            datacontenttype: "application/json".to_string(),
            id,
            pubsubname: config.pubsub_name.clone(),
            source: config.source.clone(),
            specversion: "1.0".to_string(),
            time: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            topic,
            traceid: String::new(),
            traceparent: String::new(),
            tracestate: String::new(),
            event_type: "com.dapr.event.sent".to_string(),
        }
    }
}

fn variables_to_json(source: QueryVariables) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in source {
        map.insert(key.to_string(), value.into());
    }
    map
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_change_event_serialization() {
        let mut map = Map::new();
        map.insert("id".to_string(), json!("1"));
        map.insert("value".to_string(), json!("test"));

        let event = ResultChangeEvent {
            query_id: "test-query".to_string(),
            sequence: 1,
            source_time_ms: 1609459200000,
            added_results: vec![map],
            updated_results: vec![],
            deleted_results: vec![],
            metadata: None,
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["queryId"], "test-query");
        assert_eq!(json["sequence"], 1);
        assert_eq!(json["sourceTimeMs"], 1609459200000_i64);
        assert!(json["addedResults"].is_array());
        // Empty arrays should now be included in serialization
        assert!(json["updatedResults"].is_array());
        assert_eq!(json["updatedResults"].as_array().unwrap().len(), 0);
        assert!(json["deletedResults"].is_array());
        assert_eq!(json["deletedResults"].as_array().unwrap().len(), 0);
        assert!(json.get("metadata").is_none());
    }

    #[test]
    fn test_update_payload_serialization() {
        let mut before_map = Map::new();
        before_map.insert("value".to_string(), json!(10));

        let mut after_map = Map::new();
        after_map.insert("value".to_string(), json!(20));

        let payload = UpdatePayload {
            before: Some(before_map),
            after: Some(after_map),
            grouping_keys: Some(vec!["key1".to_string()]),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["before"]["value"], 10);
        assert_eq!(json["after"]["value"], 20);
        assert_eq!(json["groupingKeys"][0], "key1");
    }

    // #[test]
    // fn test_control_event_serialization() {
    //     let event = ResultControlEvent {
    //         control_signal: ControlSignal::BootstrapStarted,
    //     };

    //     let json = serde_json::to_value(&event).unwrap();
    //     assert_eq!(json["controlSignal"], "bootstrapStarted");
    // }

    #[test]
    fn test_result_event_change_variant() {
        let change = ResultChangeEvent {
            query_id: "test".to_string(),
            sequence: 1,
            source_time_ms: 0,
            added_results: vec![],
            updated_results: vec![],
            deleted_results: vec![],
            metadata: None,
        };

        let result_event = ResultEvent::Change(change);
        let json = serde_json::to_value(&result_event).unwrap();
        assert_eq!(json["kind"], "change");
        assert_eq!(json["queryId"], "test");
    }

    // #[test]
    // fn test_result_event_control_variant() {
    //     let control = ControlEvent {
    //         control_signal: ControlSignal::Running,
    //     };

    //     let result_event = ResultEvent::Control(control);
    //     let json = serde_json::to_value(&result_event).unwrap();
    //     assert_eq!(json["kind"], "control");
    //     assert_eq!(json["controlSignal"], "running");
    // }

    // #[test]
    // fn test_cloud_event_creation() {
    //     let change = ChangeEvent {
    //         query_id: "test-query".to_string(),
    //         sequence: 1,
    //         source_time_ms: 0,
    //         added_results: vec![],
    //         updated_results: vec![],
    //         deleted_results: vec![],
    //         metadata: None,
    //     };

    //     let result_event = ResultEvent::Change(change);
    //     let config = CloudEventConfig::new();
    //     let cloud_event = CloudEvent::new(result_event, "test-query", &config);

    //     assert_eq!(cloud_event.datacontenttype, "application/json");
    //     assert_eq!(cloud_event.pubsubname, "drasi-pubsub");
    //     assert_eq!(cloud_event.source, "drasi-core");
    //     assert_eq!(cloud_event.specversion, "1.0");
    //     assert_eq!(cloud_event.topic, "test-query-results");
    //     assert_eq!(cloud_event.event_type, "com.dapr.event.sent");
    //     assert!(!cloud_event.id.is_empty());
    //     assert!(!cloud_event.time.is_empty());
    // }

    #[test]
    fn test_cloud_event_serialization() {
        let mut map = Map::new();
        map.insert("id".to_string(), json!("1"));

        let change = ResultChangeEvent {
            query_id: "test-query".to_string(),
            sequence: 1,
            source_time_ms: 0,
            added_results: vec![map],
            updated_results: vec![],
            deleted_results: vec![],
            metadata: None,
        };

        let result_event = ResultEvent::Change(change);
        let config =
            CloudEventConfig::with_values("custom-pubsub".to_string(), "custom-source".to_string());
        let cloud_event = CloudEvent::new(result_event, "test-query", &config);

        let json = serde_json::to_value(&cloud_event).unwrap();
        assert_eq!(json["datacontenttype"], "application/json");
        assert_eq!(json["pubsubname"], "custom-pubsub");
        assert_eq!(json["source"], "custom-source");
        assert_eq!(json["specversion"], "1.0");
        assert_eq!(json["topic"], "test-query-results");
        assert_eq!(json["type"], "com.dapr.event.sent");
        assert!(json["data"].is_object());
        assert_eq!(json["data"]["kind"], "change");
    }

    #[test]
    fn test_empty_arrays_included_in_serialization() {
        let event = ResultChangeEvent {
            query_id: "test".to_string(),
            sequence: 1,
            source_time_ms: 0,
            added_results: vec![],
            updated_results: vec![],
            deleted_results: vec![],
            metadata: None,
        };

        let json_str = serde_json::to_string(&event).unwrap();
        // Empty arrays should now be included
        assert!(json_str.contains("addedResults"));
        assert!(json_str.contains("updatedResults"));
        assert!(json_str.contains("deletedResults"));
        // Empty metadata should still be omitted
        assert!(!json_str.contains("metadata"));
    }

    //     #[test]
    //     fn test_metadata_serialization() {
    //         let mut metadata = HashMap::new();
    //         metadata.insert("source".to_string(), json!({"timing": 100}));

    //         let event = ResultChangeEvent {
    //             query_id: "test".to_string(),
    //             sequence: 1,
    //             source_time_ms: 0,
    //             added_results: vec![],
    //             updated_results: vec![],
    //             deleted_results: vec![],
    //             metadata: Some(metadata),
    //         };

    //         let json = serde_json::to_value(&event).unwrap();
    //         assert!(json["metadata"].is_object());
    //         assert_eq!(json["metadata"]["source"]["timing"], 100);
    //     }
}
