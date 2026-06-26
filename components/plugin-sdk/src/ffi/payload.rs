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

//! Serialized event payloads that cross the cdylib FFI boundary.
//!
//! # Why this exists (issue #602)
//!
//! `SourceEventWrapper` / `BootstrapEvent` are `repr(Rust)` types that contain
//! `bytes::Bytes` and `Arc<str>`. The previous design transferred them across the
//! cdylib boundary as opaque `Box::into_raw` pointers and reconstructed them on the
//! other side with `Box::from_raw`. That is **undefined behavior**: `repr(Rust)`
//! has no stable layout across independently compiled cdylibs, and `bytes::Bytes`
//! carries a `&'static Vtable` pointer that is only valid in the producing module's
//! address space. The result was non-deterministic heap corruption
//! (`free(): invalid pointer`).
//!
//! Instead, the producing side serializes these self-describing payloads to
//! MessagePack (via `rmp-serde`) and transfers them as raw `ptr + len` byte
//! buffers; the consuming side deserializes into its own host-owned values and
//! frees the producer's buffer through a producer-supplied `drop_fn`. No side ever
//! reads or drops the other side's `repr(Rust)` memory.
//!
//! Both the plugin SDK and the host SDK depend on this crate, so they share these
//! exact struct definitions and therefore agree on the wire schema.

use bytes::Bytes;
use chrono::DateTime;
use drasi_core::models::SourceChange;
use drasi_lib::channels::events::{BootstrapEvent, SourceEvent, SourceEventWrapper};
use serde::{Deserialize, Serialize};

/// Serialized form of a `SourceEventWrapper` for FFI transfer.
///
/// Carries everything the host needs to rebuild a host-owned
/// `SourceEventWrapper`. `profiling` is intentionally omitted: it is `None` at the
/// point a source emits an event and is populated later by the framework.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceEventPayload {
    pub source_id: String,
    pub event: SourceEvent,
    /// Event timestamp in microseconds since the Unix epoch.
    pub timestamp_us: i64,
    /// Monotonic sequence number; `None` for volatile sources.
    pub sequence: Option<u64>,
    /// Opaque, source-defined replication position bytes.
    pub source_position: Option<Vec<u8>>,
}

/// Serialized form of a `BootstrapEvent` for FFI transfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BootstrapEventPayload {
    pub source_id: String,
    pub change: SourceChange,
    /// Event timestamp in microseconds since the Unix epoch.
    pub timestamp_us: i64,
    pub sequence: u64,
}

impl SourceEventPayload {
    /// Build a payload from a `SourceEventWrapper` (producing side).
    pub fn from_wrapper(wrapper: &SourceEventWrapper) -> Self {
        let timestamp_us = wrapper
            .timestamp
            .timestamp_nanos_opt()
            .map(|n| n / 1000)
            .unwrap_or(0);
        Self {
            source_id: wrapper.source_id.clone(),
            event: wrapper.event.clone(),
            timestamp_us,
            sequence: wrapper.sequence,
            source_position: wrapper.source_position.as_ref().map(|b| b.to_vec()),
        }
    }

    /// Reconstruct a host/plugin-owned `SourceEventWrapper` (consuming side).
    pub fn into_wrapper(self) -> SourceEventWrapper {
        let timestamp =
            DateTime::from_timestamp_micros(self.timestamp_us).unwrap_or_else(chrono::Utc::now);
        SourceEventWrapper {
            source_id: self.source_id,
            event: self.event,
            timestamp,
            profiling: None,
            sequence: self.sequence,
            source_position: self.source_position.map(Bytes::from),
        }
    }
}

impl BootstrapEventPayload {
    /// Build a payload from a `BootstrapEvent` (producing side).
    pub fn from_event(record: &BootstrapEvent) -> Self {
        let timestamp_us = record
            .timestamp
            .timestamp_nanos_opt()
            .map(|n| n / 1000)
            .unwrap_or(0);
        Self {
            source_id: record.source_id.clone(),
            change: record.change.clone(),
            timestamp_us,
            sequence: record.sequence,
        }
    }

    /// Reconstruct a host/plugin-owned `BootstrapEvent` (consuming side).
    pub fn into_event(self) -> BootstrapEvent {
        let timestamp =
            DateTime::from_timestamp_micros(self.timestamp_us).unwrap_or_else(chrono::Utc::now);
        BootstrapEvent {
            source_id: self.source_id,
            change: self.change,
            timestamp,
            sequence: self.sequence,
        }
    }
}

/// Decode raw MessagePack FFI payload bytes into a `SourceEventWrapper`.
/// Returns `None` (and logs) on decode failure.
pub fn decode_source_event_payload(bytes: &[u8]) -> Option<SourceEventWrapper> {
    match rmp_serde::from_slice::<SourceEventPayload>(bytes) {
        Ok(p) => Some(p.into_wrapper()),
        Err(e) => {
            log::error!("Failed to decode FFI source event payload: {e}");
            None
        }
    }
}

/// Decode raw MessagePack FFI payload bytes into a `BootstrapEvent`.
/// Returns `None` (and logs) on decode failure.
pub fn decode_bootstrap_event_payload(bytes: &[u8]) -> Option<BootstrapEvent> {
    match rmp_serde::from_slice::<BootstrapEventPayload>(bytes) {
        Ok(p) => Some(p.into_event()),
        Err(e) => {
            log::error!("Failed to decode FFI bootstrap event payload: {e}");
            None
        }
    }
}

/// Encode a `QueryResult` to MessagePack bytes for host→plugin FFI transfer.
pub fn encode_query_result(result: &drasi_lib::channels::QueryResult) -> Vec<u8> {
    // `to_vec_named` (field-name map) tolerates `#[serde(skip_serializing_if)]`
    // fields such as `QueryResult::profiling`; the compact positional encoding
    // would emit fewer elements than the decoder expects.
    match rmp_serde::to_vec_named(result) {
        Ok(b) => b,
        Err(e) => {
            log::error!("Failed to encode FFI query result payload: {e}");
            Vec::new()
        }
    }
}

/// Decode raw MessagePack FFI payload bytes into a `QueryResult`.
/// Returns `None` (and logs) on decode failure.
pub fn decode_query_result(bytes: &[u8]) -> Option<drasi_lib::channels::QueryResult> {
    match rmp_serde::from_slice::<drasi_lib::channels::QueryResult>(bytes) {
        Ok(r) => Some(r),
        Err(e) => {
            log::error!("Failed to decode FFI query result payload: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    //! T2 (part 1) — payload round-trip fidelity and the named-encoding
    //! regression guard for issue #602's cross-cdylib serialized transfer.

    use super::*;
    use bytes::Bytes;
    use chrono::Utc;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use drasi_lib::channels::events::SourceEvent;
    use std::sync::Arc;

    fn sample_node() -> Element {
        let mut props = ElementPropertyMap::new();
        props.insert("plate", ElementValue::String(Arc::from("A1234")));
        props.insert("speed", ElementValue::Integer(55));
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("src-1", "vehicles:A1234"),
                labels: Arc::from(vec![Arc::from("vehicles")]),
                effective_from: 1_771_000_000_000,
            },
            properties: props,
        }
    }

    #[test]
    fn source_event_payload_roundtrips_via_named_encoding() {
        let wrapper = SourceEventWrapper {
            source_id: "src-1".to_string(),
            event: SourceEvent::Change(SourceChange::Insert {
                element: sample_node(),
            }),
            timestamp: Utc::now(),
            profiling: None,
            sequence: Some(42),
            source_position: Some(Bytes::from_static(b"binlog:000003:1766")),
        };

        let payload = SourceEventPayload::from_wrapper(&wrapper);
        let bytes = rmp_serde::to_vec_named(&payload).expect("serialize");
        let decoded = decode_source_event_payload(&bytes).expect("decode");

        assert_eq!(decoded.source_id, wrapper.source_id);
        assert_eq!(decoded.sequence, wrapper.sequence);
        assert_eq!(decoded.event, wrapper.event);
        assert_eq!(
            decoded.source_position.as_deref(),
            Some(&b"binlog:000003:1766"[..])
        );
        assert_eq!(
            decoded.timestamp.timestamp_micros(),
            wrapper.timestamp.timestamp_micros()
        );
    }

    #[test]
    fn bootstrap_event_payload_roundtrips() {
        let record = BootstrapEvent {
            source_id: "src-1".to_string(),
            change: SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("src-1", "vehicles:B5678"),
                    labels: Arc::from(vec![Arc::from("vehicles")]),
                    effective_from: 9,
                },
            },
            timestamp: Utc::now(),
            sequence: 7,
        };

        let payload = BootstrapEventPayload::from_event(&record);
        let bytes = rmp_serde::to_vec_named(&payload).expect("serialize");
        let decoded = decode_bootstrap_event_payload(&bytes).expect("decode");

        assert_eq!(decoded.source_id, record.source_id);
        assert_eq!(decoded.sequence, record.sequence);
        assert_eq!(decoded.change, record.change);
    }

    #[test]
    fn query_result_roundtrips_with_skipped_profiling_field() {
        // Regression guard: `QueryResult::profiling` is
        // `#[serde(skip_serializing_if = "Option::is_none")]`. The compact
        // positional MessagePack encoding emits 5 elements for a 6-field struct
        // and fails to decode ("invalid length 5, expected 6"); named encoding
        // (`encode_query_result`) tolerates the skipped field.
        let qr = drasi_lib::channels::QueryResult::new(
            "q-1".to_string(),
            3,
            Utc::now(),
            Vec::new(),
            std::collections::HashMap::new(),
        );
        assert!(qr.profiling.is_none());

        let bytes = encode_query_result(&qr);
        let decoded = decode_query_result(&bytes).expect("decode QueryResult");

        assert_eq!(decoded.query_id, qr.query_id);
        assert_eq!(decoded.sequence, qr.sequence);
        assert!(decoded.profiling.is_none());
    }
}
