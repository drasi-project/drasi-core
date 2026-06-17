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

//! Wire-format types emitted by the HTTP reaction.
//!
//! These types describe the JSON payloads the reaction sends to its
//! receivers. They are the *output* counterpart to the config DTOs in
//! [`crate::descriptor`] and are the source of truth for the JSON
//! Schema published at `schema/output.schema.json`.
//!
//! Two payload shapes are emitted:
//!
//! * [`DefaultChangeNotification`] — the body POSTed for each result
//!   change when no per-query body template applies. Used both for
//!   the synthesized default endpoint (`POST {baseUrl}/changes/{queryId}`)
//!   and as the render-error fallback when a user template fails.
//! * [`BatchEnvelope`] — the Pattern C batch container POSTed to a
//!   configured `batchEndpoint` in adaptive mode. Its `batch` field is a
//!   JSON array of [`DefaultChangeNotification`] items.
//!
//! `ResultDiff::Aggregation` is folded into [`Operation::Update`] (matching
//! the gRPC, Azure Storage, and RabbitMQ reactions). `ResultDiff::Noop`
//! produces no notification; the caller skips it without emitting a
//! request.

use drasi_lib::channels::{QueryResult, ResultDiff};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::config::OperationType;

/// The kind of change carried in a [`DefaultChangeNotification`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Operation {
    /// A new row appeared in the query result.
    Add,
    /// A row's values changed (also used for `Aggregation` updates).
    Update,
    /// A row left the query result.
    Delete,
}

/// Default JSON envelope emitted by the HTTP reaction for a single
/// query-result change.
///
/// Sent as the request body to either the synthesized default endpoint
/// (`POST {baseUrl}/changes/{queryId}`) or to a per-query route whose
/// template body is empty.
///
/// # Field population
///
/// | `ResultDiff` variant      | `operation` | `before`                              | `after`           |
/// |---------------------------|-------------|---------------------------------------|-------------------|
/// | `Add { data }`            | `ADD`       | omitted                               | `Some(data)`      |
/// | `Delete { data }`         | `DELETE`    | `Some(data)`                          | omitted           |
/// | `Update { before, after }`| `UPDATE`    | `Some(before)`                        | `Some(after)`     |
/// | `Aggregation { before, after }` | `UPDATE` | `before` (omitted on first emission) | `Some(after)`     |
/// | `Noop`                    | _no notification — caller skips_                                              |||
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefaultChangeNotification {
    /// What kind of change this notification represents.
    pub operation: Operation,
    /// The continuous query that produced the change.
    pub query_id: String,
    /// Monotonic per-query sequence number identifying this emission.
    pub sequence_id: u64,
    /// RFC 3339 timestamp of the originating query emission (UTC).
    pub timestamp: String,
    /// Row state **before** the change.
    /// Omitted for `ADD` and for the first emission of an aggregation group.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub before: Option<serde_json::Value>,
    /// Row state **after** the change. Omitted for `DELETE`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub after: Option<serde_json::Value>,
    /// Source/query metadata carried by the originating `QueryResult`.
    /// Omitted when empty.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<serde_json::Value>,
    /// Raw `data` payload of an `Update` diff, retained solely to populate
    /// the `data` template-context key. Never serialized on the wire.
    #[serde(skip)]
    pub(crate) raw_data: Option<serde_json::Value>,
}

impl DefaultChangeNotification {
    /// Build a notification from a `QueryResult` and one of its diffs.
    /// Returns `None` for `ResultDiff::Noop` so callers can drop it
    /// without emitting a request.
    pub fn from_diff(query_result: &QueryResult, diff: &ResultDiff) -> Option<Self> {
        let timestamp = query_result.timestamp.to_rfc3339();
        let metadata = if query_result.metadata.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(
                query_result.metadata.clone().into_iter().collect(),
            ))
        };
        let (operation, before, after, raw_data) = match diff {
            ResultDiff::Add { data, .. } => (Operation::Add, None, Some(data.clone()), None),
            ResultDiff::Delete { data, .. } => (Operation::Delete, Some(data.clone()), None, None),
            ResultDiff::Update {
                data,
                before,
                after,
                ..
            } => (
                Operation::Update,
                Some(before.clone()),
                Some(after.clone()),
                Some(data.clone()),
            ),
            ResultDiff::Aggregation { before, after, .. } => {
                (Operation::Update, before.clone(), Some(after.clone()), None)
            }
            ResultDiff::Noop => return None,
        };
        Some(Self {
            operation,
            query_id: query_result.query_id.clone(),
            sequence_id: query_result.sequence,
            timestamp,
            before,
            after,
            metadata,
            raw_data,
        })
    }

    /// String form of the operation as used by the Handlebars context
    /// dispatcher (`build_context` in `process.rs`).
    pub(crate) fn op_str(&self) -> &'static str {
        match self.operation {
            Operation::Add => "ADD",
            Operation::Update => "UPDATE",
            Operation::Delete => "DELETE",
        }
    }

    /// Map to the internal [`OperationType`] used by the template router
    /// (`HttpReactionConfig::resolve_call_spec`).
    pub(crate) fn operation_type(&self) -> OperationType {
        match self.operation {
            Operation::Add => OperationType::Add,
            Operation::Update => OperationType::Update,
            Operation::Delete => OperationType::Delete,
        }
    }
}

/// Wire payload POSTed to a configured `batchEndpoint` in adaptive mode.
///
/// Pattern C (batched envelopes) from the reaction developer guide: a
/// single container object whose `batch` field is a JSON array of
/// [`DefaultChangeNotification`] items (Pattern A items), each carrying
/// its own `queryId` / `sequenceId` / `timestamp`. The array is always
/// present even when it holds a single item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchEnvelope {
    /// The coalesced change notifications, in order.
    pub batch: Vec<DefaultChangeNotification>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::collections::HashMap;

    fn fixed_ts() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn qr_with(
        sequence: u64,
        metadata: HashMap<String, serde_json::Value>,
        diff: ResultDiff,
    ) -> QueryResult {
        QueryResult::new("q".to_string(), sequence, fixed_ts(), vec![diff], metadata)
    }

    fn single(diff: ResultDiff) -> QueryResult {
        qr_with(0, HashMap::new(), diff)
    }

    #[test]
    fn add_maps_to_add_with_after_only() {
        let qr = single(ResultDiff::Add {
            data: json!({"id": 1}),
            row_signature: 0,
        });
        let n = DefaultChangeNotification::from_diff(&qr, &qr.results[0]).unwrap();
        assert_eq!(n.operation, Operation::Add);
        assert_eq!(n.before, None);
        assert_eq!(n.after, Some(json!({"id": 1})));
        assert_eq!(n.raw_data, None);
        // Serialization omits absent fields.
        let v = serde_json::to_value(&n).unwrap();
        assert!(v.get("before").is_none(), "before should be omitted");
        assert_eq!(v.get("operation"), Some(&json!("ADD")));
        assert_eq!(v.get("queryId"), Some(&json!("q")));
        assert_eq!(v.get("sequenceId"), Some(&json!(0)));
        assert!(
            v.get("metadata").is_none(),
            "empty metadata should be omitted"
        );
        assert!(
            v.get("raw_data").is_none() && v.get("rawData").is_none(),
            "raw_data must never serialize on the wire"
        );
    }

    #[test]
    fn delete_maps_to_delete_with_before_only() {
        let qr = single(ResultDiff::Delete {
            data: json!({"id": 1}),
            row_signature: 0,
        });
        let n = DefaultChangeNotification::from_diff(&qr, &qr.results[0]).unwrap();
        assert_eq!(n.operation, Operation::Delete);
        assert_eq!(n.before, Some(json!({"id": 1})));
        assert_eq!(n.after, None);
        let v = serde_json::to_value(&n).unwrap();
        assert!(v.get("after").is_none(), "after should be omitted");
        assert_eq!(v.get("operation"), Some(&json!("DELETE")));
    }

    #[test]
    fn update_maps_to_update_with_before_after_and_raw_data() {
        let qr = single(ResultDiff::Update {
            data: json!({"d": 1}),
            before: json!({"v": 1}),
            after: json!({"v": 2}),
            grouping_keys: None,
            row_signature: 0,
        });
        let n = DefaultChangeNotification::from_diff(&qr, &qr.results[0]).unwrap();
        assert_eq!(n.operation, Operation::Update);
        assert_eq!(n.before, Some(json!({"v": 1})));
        assert_eq!(n.after, Some(json!({"v": 2})));
        // The raw Update `data` is retained for the `{{data}}` context key.
        assert_eq!(n.raw_data, Some(json!({"d": 1})));
    }

    #[test]
    fn aggregation_maps_to_update_carrying_optional_before() {
        let first = single(ResultDiff::Aggregation {
            before: None,
            after: json!({"count": 1}),
            row_signature: 0,
        });
        let n = DefaultChangeNotification::from_diff(&first, &first.results[0]).unwrap();
        assert_eq!(n.operation, Operation::Update);
        assert_eq!(n.before, None);
        let v = serde_json::to_value(&n).unwrap();
        assert!(
            v.get("before").is_none(),
            "first aggregation emission omits before"
        );

        let change = single(ResultDiff::Aggregation {
            before: Some(json!({"count": 1})),
            after: json!({"count": 2}),
            row_signature: 0,
        });
        let n = DefaultChangeNotification::from_diff(&change, &change.results[0]).unwrap();
        assert_eq!(n.operation, Operation::Update);
        assert_eq!(n.before, Some(json!({"count": 1})));
        assert_eq!(n.after, Some(json!({"count": 2})));
    }

    #[test]
    fn noop_yields_none() {
        let qr = single(ResultDiff::Noop);
        assert!(DefaultChangeNotification::from_diff(&qr, &qr.results[0]).is_none());
    }

    #[test]
    fn sequence_and_metadata_are_threaded_onto_the_envelope() {
        let mut metadata = HashMap::new();
        metadata.insert("source".to_string(), json!("sensors"));
        let qr = qr_with(
            42,
            metadata,
            ResultDiff::Add {
                data: json!({"id": 1}),
                row_signature: 0,
            },
        );
        let n = DefaultChangeNotification::from_diff(&qr, &qr.results[0]).unwrap();
        assert_eq!(n.sequence_id, 42);
        assert_eq!(n.metadata, Some(json!({"source": "sensors"})));
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v.get("sequenceId"), Some(&json!(42)));
        assert_eq!(v.get("metadata"), Some(&json!({"source": "sensors"})));
    }

    #[test]
    fn batch_envelope_serializes_with_batch_array() {
        let qr = single(ResultDiff::Add {
            data: json!({"id": 1}),
            row_signature: 0,
        });
        let n = DefaultChangeNotification::from_diff(&qr, &qr.results[0]).unwrap();
        let env = BatchEnvelope { batch: vec![n] };
        let v = serde_json::to_value(&env).unwrap();
        let batch = v.get("batch").and_then(|b| b.as_array());
        assert!(batch.is_some(), "batch must be a JSON array");
        assert_eq!(batch.unwrap().len(), 1);
        assert_eq!(v["batch"][0].get("operation"), Some(&json!("ADD")));
        assert_eq!(
            v.as_object().map(|o| o.len()),
            Some(1),
            "the container has only the 'batch' key"
        );
    }
}
