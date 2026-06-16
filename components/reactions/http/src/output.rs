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
//! * [`BatchResult`] — the JSON array element posted to a configured
//!   `batchEndpoint` in adaptive mode. Each element groups one query's
//!   coalesced [`DefaultChangeNotification`]s.
//!
//! `ResultDiff::Aggregation` is folded into [`Operation::Update`] (matching
//! the gRPC, Azure Storage, and RabbitMQ reactions). `ResultDiff::Noop`
//! produces no notification; the caller skips it without emitting a
//! request.

use chrono::{DateTime, Utc};
use drasi_lib::channels::ResultDiff;
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
    /// RFC 3339 timestamp of the originating query emission (UTC).
    pub timestamp: String,
    /// Row state **before** the change.
    /// Omitted for `ADD` and for the first emission of an aggregation group.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub before: Option<serde_json::Value>,
    /// Row state **after** the change. Omitted for `DELETE`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub after: Option<serde_json::Value>,
}

impl DefaultChangeNotification {
    /// Build a notification from a `ResultDiff`. Returns `None` for
    /// `ResultDiff::Noop` so callers can drop it without emitting a
    /// request.
    pub fn from_diff(query_id: &str, timestamp: DateTime<Utc>, diff: &ResultDiff) -> Option<Self> {
        let ts = timestamp.to_rfc3339();
        match diff {
            ResultDiff::Add { data, .. } => Some(Self {
                operation: Operation::Add,
                query_id: query_id.to_string(),
                timestamp: ts,
                before: None,
                after: Some(data.clone()),
            }),
            ResultDiff::Delete { data, .. } => Some(Self {
                operation: Operation::Delete,
                query_id: query_id.to_string(),
                timestamp: ts,
                before: Some(data.clone()),
                after: None,
            }),
            ResultDiff::Update { before, after, .. } => Some(Self {
                operation: Operation::Update,
                query_id: query_id.to_string(),
                timestamp: ts,
                before: Some(before.clone()),
                after: Some(after.clone()),
            }),
            ResultDiff::Aggregation { before, after, .. } => Some(Self {
                operation: Operation::Update,
                query_id: query_id.to_string(),
                timestamp: ts,
                before: before.clone(),
                after: Some(after.clone()),
            }),
            ResultDiff::Noop => None,
        }
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
    /// (`HttpReactionConfig::get_template_spec`).
    pub(crate) fn operation_type(&self) -> OperationType {
        match self.operation {
            Operation::Add => OperationType::Add,
            Operation::Update => OperationType::Update,
            Operation::Delete => OperationType::Delete,
        }
    }

    /// Value passed as `data` to [`crate::process::process_result`] so that
    /// the Handlebars context (`{{after}}` / `{{before}}` / `{{data}}`)
    /// resolves consistently with the operation. ADD exposes the row as
    /// `after`; DELETE exposes the row as `before`; UPDATE/Aggregation
    /// exposes `{ before, after }`.
    pub(crate) fn handlebars_data(&self) -> serde_json::Value {
        match self.operation {
            Operation::Add => self.after.clone().unwrap_or(serde_json::Value::Null),
            Operation::Delete => self.before.clone().unwrap_or(serde_json::Value::Null),
            Operation::Update => {
                let mut obj = serde_json::Map::new();
                if let Some(b) = &self.before {
                    obj.insert("before".to_string(), b.clone());
                }
                if let Some(a) = &self.after {
                    obj.insert("after".to_string(), a.clone());
                }
                serde_json::Value::Object(obj)
            }
        }
    }
}

/// One element of the coalesced batch array POSTed to a configured
/// `batchEndpoint` in adaptive mode.
///
/// Each element groups one query's results from the batch. The wire
/// payload sent to the batch endpoint is a JSON array of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    /// The continuous query that produced the results in this group.
    pub query_id: String,
    /// The coalesced change notifications for this query, in order.
    pub results: Vec<DefaultChangeNotification>,
    /// RFC 3339 timestamp of when the batch was assembled.
    pub timestamp: String,
    /// Number of entries in [`Self::results`].
    pub count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn fixed_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn add_maps_to_add_with_after_only() {
        let diff = ResultDiff::Add {
            data: json!({"id": 1}),
            row_signature: 0,
        };
        let n = DefaultChangeNotification::from_diff("q", fixed_ts(), &diff).unwrap();
        assert_eq!(n.operation, Operation::Add);
        assert_eq!(n.before, None);
        assert_eq!(n.after, Some(json!({"id": 1})));
        // Serialization omits absent fields.
        let v = serde_json::to_value(&n).unwrap();
        assert!(v.get("before").is_none(), "before should be omitted");
        assert_eq!(v.get("operation"), Some(&json!("ADD")));
        assert_eq!(v.get("queryId"), Some(&json!("q")));
    }

    #[test]
    fn delete_maps_to_delete_with_before_only() {
        let diff = ResultDiff::Delete {
            data: json!({"id": 1}),
            row_signature: 0,
        };
        let n = DefaultChangeNotification::from_diff("q", fixed_ts(), &diff).unwrap();
        assert_eq!(n.operation, Operation::Delete);
        assert_eq!(n.before, Some(json!({"id": 1})));
        assert_eq!(n.after, None);
        let v = serde_json::to_value(&n).unwrap();
        assert!(v.get("after").is_none(), "after should be omitted");
        assert_eq!(v.get("operation"), Some(&json!("DELETE")));
    }

    #[test]
    fn update_maps_to_update_with_before_and_after() {
        let diff = ResultDiff::Update {
            data: json!({}),
            before: json!({"v": 1}),
            after: json!({"v": 2}),
            grouping_keys: None,
            row_signature: 0,
        };
        let n = DefaultChangeNotification::from_diff("q", fixed_ts(), &diff).unwrap();
        assert_eq!(n.operation, Operation::Update);
        assert_eq!(n.before, Some(json!({"v": 1})));
        assert_eq!(n.after, Some(json!({"v": 2})));
    }

    #[test]
    fn aggregation_maps_to_update_carrying_optional_before() {
        let first = ResultDiff::Aggregation {
            before: None,
            after: json!({"count": 1}),
            row_signature: 0,
        };
        let n = DefaultChangeNotification::from_diff("q", fixed_ts(), &first).unwrap();
        assert_eq!(n.operation, Operation::Update);
        assert_eq!(n.before, None);
        let v = serde_json::to_value(&n).unwrap();
        assert!(
            v.get("before").is_none(),
            "first aggregation emission omits before"
        );

        let change = ResultDiff::Aggregation {
            before: Some(json!({"count": 1})),
            after: json!({"count": 2}),
            row_signature: 0,
        };
        let n = DefaultChangeNotification::from_diff("q", fixed_ts(), &change).unwrap();
        assert_eq!(n.operation, Operation::Update);
        assert_eq!(n.before, Some(json!({"count": 1})));
        assert_eq!(n.after, Some(json!({"count": 2})));
    }

    #[test]
    fn noop_yields_none() {
        let diff = ResultDiff::Noop;
        assert!(DefaultChangeNotification::from_diff("q", fixed_ts(), &diff).is_none());
    }

    #[test]
    fn batch_result_serializes_with_camelcase_keys() {
        let br = BatchResult {
            query_id: "q1".into(),
            results: vec![],
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            count: 0,
        };
        let v = serde_json::to_value(&br).unwrap();
        assert!(v.get("queryId").is_some());
        assert!(v.get("results").is_some());
        assert!(v.get("timestamp").is_some());
        assert!(v.get("count").is_some());
        assert!(v.get("query_id").is_none(), "snake_case keys must be gone");
    }
}
