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

//! Wire-format types emitted by the Event Grid reaction.
//!
//! The reaction serializes each event to a JSON array element, in one of two
//! schemas selected by [`crate::config::EventGridSchema`]:
//!
//! * **CloudEvents 1.0** — `{ id, source, type, specversion, time, subject,
//!   data, <extensions...> }`. Template `metadata` is flattened into the object
//!   as extension attributes.
//! * **Native Event Grid** — `{ id, subject, eventType, eventTime, data,
//!   dataVersion }`. Has no extension-attribute concept, so `metadata` is
//!   dropped (a warning is logged by the caller).
//!
//! Event ids are **deterministic** (UUIDv5 from the reaction id + query id +
//! sequence + op + item index) so retries and outbox replays carry the same id,
//! enabling downstream deduplication.

use drasi_lib::channels::{QueryResult, ResultDiff};
use serde_json::{json, Map, Value};

use crate::config::EventGridSchema;

/// Event type stamped on change events.
pub const CHANGE_EVENT_TYPE: &str = "Drasi.ChangeEvent";

/// Data version stamped on native Event Grid events.
pub const DATA_VERSION: &str = "1";

/// Namespace UUID for deterministic event-id derivation (random, fixed).
const EVENT_ID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x8f, 0x3a, 0x1c, 0x74, 0x2e, 0x11, 0x4a, 0x5b, 0x9c, 0x2d, 0x44, 0xbf, 0x36, 0xb1, 0x0c, 0xd2,
]);

/// Change operation code used in the unpacked notification `op` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    /// Insert (new row).
    Insert,
    /// Update (modified row).
    Update,
    /// Delete (removed row).
    Delete,
}

impl ChangeOp {
    /// Single-character op code (`i`/`u`/`d`), matching the platform.
    pub fn code(&self) -> &'static str {
        match self {
            ChangeOp::Insert => "i",
            ChangeOp::Update => "u",
            ChangeOp::Delete => "d",
        }
    }
}

/// Derive a deterministic event id from the identifying tuple.
pub fn deterministic_id(
    reaction_id: &str,
    query_id: &str,
    sequence: u64,
    op: &str,
    index: usize,
) -> String {
    let name = format!("{reaction_id}|{query_id}|{sequence}|{op}|{index}");
    uuid::Uuid::new_v5(&EVENT_ID_NAMESPACE, name.as_bytes()).to_string()
}

/// A schema-agnostic event ready to be encoded into either wire schema.
#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope {
    /// Deterministic event id.
    pub id: String,
    /// Subject / source (the query id).
    pub subject: String,
    /// Event type (e.g. `Drasi.ChangeEvent`).
    pub event_type: String,
    /// RFC 3339 timestamp.
    pub time: String,
    /// Event payload.
    pub data: Value,
    /// Extension attributes (CloudEvents only); dropped for native EventGrid.
    pub metadata: Map<String, Value>,
}

impl EventEnvelope {
    /// Encode this event into a JSON value for the given wire `schema`.
    ///
    /// Returns `(value, dropped_metadata)` where `dropped_metadata` is `true`
    /// when non-empty metadata was discarded because the schema has no
    /// extension-attribute concept (native EventGrid).
    pub fn to_value(&self, schema: EventGridSchema) -> (Value, bool) {
        match schema {
            EventGridSchema::CloudEvents => {
                let mut obj = Map::new();
                obj.insert("id".into(), json!(self.id));
                obj.insert("source".into(), json!(self.subject));
                obj.insert("type".into(), json!(self.event_type));
                obj.insert("specversion".into(), json!("1.0"));
                obj.insert("time".into(), json!(self.time));
                obj.insert("subject".into(), json!(self.subject));
                obj.insert("data".into(), self.data.clone());
                // Flatten metadata as extension attributes. CloudEvents requires
                // attribute names to be lowercase, so normalize keys before the
                // reserved-key check and insertion.
                for (k, v) in &self.metadata {
                    let key = k.to_ascii_lowercase();
                    if !is_reserved_cloudevents_key(&key) {
                        obj.insert(key, v.clone());
                    }
                }
                (Value::Object(obj), false)
            }
            EventGridSchema::EventGrid => {
                let value = json!({
                    "id": self.id,
                    "subject": self.subject,
                    "eventType": self.event_type,
                    "eventTime": self.time,
                    "data": self.data,
                    "dataVersion": DATA_VERSION,
                });
                (value, !self.metadata.is_empty())
            }
        }
    }
}

fn is_reserved_cloudevents_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "source" | "type" | "specversion" | "time" | "subject" | "data" | "dataschema"
    )
}

/// Map a [`ResultDiff`] to `(ChangeOp, data-value)` for **unpacked** output.
///
/// Returns `None` for `ResultDiff::Noop`.
pub fn unpacked_notification(
    query_result: &QueryResult,
    diff: &ResultDiff,
) -> Option<(ChangeOp, Value)> {
    let ts_ms = query_result.timestamp.timestamp_millis();
    let (op, before, after) = match diff {
        ResultDiff::Add { data, .. } => (ChangeOp::Insert, None, Some(data.clone())),
        ResultDiff::Delete { data, .. } => (ChangeOp::Delete, Some(data.clone()), None),
        ResultDiff::Update { before, after, .. } => {
            (ChangeOp::Update, Some(before.clone()), Some(after.clone()))
        }
        ResultDiff::Aggregation { before, after, .. } => {
            (ChangeOp::Update, before.clone(), Some(after.clone()))
        }
        ResultDiff::Noop => return None,
    };

    let mut payload = Map::new();
    payload.insert(
        "source".into(),
        json!({ "queryId": query_result.query_id, "ts_ms": ts_ms }),
    );
    if let Some(before) = before {
        payload.insert("before".into(), before);
    }
    if let Some(after) = after {
        payload.insert("after".into(), after);
    }

    let data = json!({
        "op": op.code(),
        "ts_ms": ts_ms,
        "payload": Value::Object(payload),
    });

    Some((op, data))
}
