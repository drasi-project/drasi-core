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

use super::vtables::{FfiBootstrapEvent, FfiQueryResult, FfiSourceEvent};
use bytes::Bytes;
use chrono::DateTime;
use drasi_core::models::SourceChange;
use drasi_lib::channels::events::{BootstrapEvent, SourceEvent, SourceEventWrapper};
use serde::{Deserialize, Serialize};

/// Maximum accepted serialized FFI payload size.
///
/// Defends the consuming process against unbounded allocation from a buggy or
/// malicious peer that supplies an enormous `payload_len`. 256 MiB is far larger
/// than any realistic single change event / query result, so legitimate traffic
/// is never affected. This bound also caps the `source_position` bytes, which are
/// carried inside the payload.
pub const MAX_FFI_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

/// Read a peer-owned serialized payload buffer, decode it, and free the peer's
/// buffer via `drop_fn`.
///
/// This is the single, hardened entry point used by **every** FFI payload
/// consumer (source events, bootstrap events, query results, on both the host and
/// plugin sides). It defends against a buggy or malicious producer:
///
/// - null / empty buffer → returns `None`;
/// - `len > MAX_FFI_PAYLOAD_BYTES` → rejected (no allocation) to prevent a
///   memory-exhaustion DoS;
/// - **null `drop_fn`** → the buffer is leaked with a logged error instead of
///   calling a null function pointer (which would crash the process — there is no
///   panic recovery across `extern "C"`). The ABI requires `drop_fn` to be
///   non-null; this guard is defense-in-depth.
///
/// The producer's buffer is always freed when `drop_fn` is non-null, even if
/// decoding fails.
///
/// # Safety
/// `ptr` / `len` must describe a buffer produced by the peer's serializer that is
/// owned by the peer and freeable by `drop_fn`.
pub unsafe fn take_ffi_payload<T>(
    ptr: *const u8,
    len: usize,
    drop_fn: Option<extern "C" fn(*mut u8, usize)>,
    decode: impl FnOnce(&[u8]) -> Option<T>,
) -> Option<T> {
    let decoded = if ptr.is_null() || len == 0 {
        None
    } else if len > MAX_FFI_PAYLOAD_BYTES {
        log::error!("Rejecting oversized FFI payload ({len} bytes > {MAX_FFI_PAYLOAD_BYTES} max)");
        None
    } else {
        decode(unsafe { std::slice::from_raw_parts(ptr, len) })
    };
    // Free the producer's buffer. A null `drop_fn` (ABI violation by a buggy or
    // malicious producer) leaks the buffer with a logged error rather than
    // crashing the process — there is no panic recovery across `extern "C"`.
    if !ptr.is_null() {
        match drop_fn {
            Some(free) => free(ptr as *mut u8, len),
            None => {
                log::error!("FFI contract violation: null payload_drop_fn; leaking {len} bytes")
            }
        }
    }
    decoded
}

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
        let timestamp = DateTime::from_timestamp_micros(self.timestamp_us)
            // Deterministic fallback (Unix epoch) on an out-of-range/invalid
            // timestamp — never `Utc::now()`, which would be non-deterministic
            // and could silently mask a corrupt payload.
            .unwrap_or(DateTime::UNIX_EPOCH);
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
        let timestamp = DateTime::from_timestamp_micros(self.timestamp_us)
            // Deterministic fallback (Unix epoch) on an out-of-range/invalid
            // timestamp — never `Utc::now()`, which would be non-deterministic
            // and could silently mask a corrupt payload.
            .unwrap_or(DateTime::UNIX_EPOCH);
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
    //
    // Serialization of this in-memory, properly-derived type cannot fail at
    // runtime (no I/O; no fallible custom `Serialize` impl). If it ever does it
    // is a programming error — a non-serializable field was added. Returning an
    // empty buffer would be silently decoded by the consumer as "no result",
    // i.e. undetectable data loss in a reactive pipeline, so we fail loudly.
    rmp_serde::to_vec_named(result)
        .expect("BUG: failed to encode FFI query result payload (non-serializable field added?)")
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

/// Consume a peer-produced `FfiSourceEvent`: decode its serialized payload into a
/// host-owned `SourceEventWrapper` and free the peer's payload buffer via the
/// envelope's `payload_drop_fn` (delegating null/size hardening to
/// [`take_ffi_payload`]). The `#[repr(C)]` envelope itself is borrowed and not
/// freed here.
///
/// This is the single canonical source-event consumer shared by every FFI
/// consumer site (host and plugin), so the decode→free→hardening sequence is
/// implemented exactly once (issue #602).
///
/// # Safety
/// `ffi` must reference a valid envelope produced by the peer's serializer, whose
/// `payload_ptr`/`payload_len`/`payload_drop_fn` describe a peer-owned buffer.
pub unsafe fn consume_source_event(ffi: &FfiSourceEvent) -> Option<SourceEventWrapper> {
    unsafe {
        take_ffi_payload(
            ffi.payload_ptr,
            ffi.payload_len,
            ffi.payload_drop_fn,
            decode_source_event_payload,
        )
    }
}

/// Consume a peer-produced `FfiBootstrapEvent`: decode its serialized payload into
/// a host-owned `BootstrapEvent` and free the peer's payload buffer via the
/// envelope's `payload_drop_fn`. The `#[repr(C)]` envelope itself is borrowed and
/// not freed here. Single canonical bootstrap-event consumer (issue #602).
///
/// # Safety
/// See [`consume_source_event`].
pub unsafe fn consume_bootstrap_event(ffi: &FfiBootstrapEvent) -> Option<BootstrapEvent> {
    unsafe {
        take_ffi_payload(
            ffi.payload_ptr,
            ffi.payload_len,
            ffi.payload_drop_fn,
            decode_bootstrap_event_payload,
        )
    }
}

/// Consume a peer-produced `*mut FfiQueryResult`: decode its serialized payload
/// into an owned `QueryResult`, free the peer's payload buffer via the envelope's
/// `payload_drop_fn`, **and** free the `#[repr(C)]` envelope `Box` itself (whose
/// ownership the producer transferred via `Box::into_raw`). Single canonical
/// query-result consumer (issue #602).
///
/// Unlike [`consume_source_event`]/[`consume_bootstrap_event`] (which borrow their
/// envelope), the query-result envelope is passed by owned pointer, so this also
/// reclaims it.
///
/// # Safety
/// `ptr` must be a non-null `*mut FfiQueryResult` produced by the peer via
/// `Box::into_raw`, whose `payload_ptr`/`payload_len`/`payload_drop_fn` describe a
/// peer-owned buffer.
pub unsafe fn consume_query_result(
    ptr: *mut FfiQueryResult,
) -> Option<drasi_lib::channels::QueryResult> {
    let ffi = unsafe { &*ptr };
    let decoded = unsafe {
        take_ffi_payload(
            ffi.payload_ptr,
            ffi.payload_len,
            ffi.payload_drop_fn,
            decode_query_result,
        )
    };
    unsafe { drop(Box::from_raw(ptr)) };
    decoded
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

    // ---- take_ffi_payload hardening (security review) ----

    use std::sync::atomic::{AtomicUsize, Ordering};

    // Dedicated per-test counters (no shared static / `store(0)` resets that would
    // race under parallel test execution).
    static DROPS_DECODE: AtomicUsize = AtomicUsize::new(0);
    static DROPS_OVERSIZED: AtomicUsize = AtomicUsize::new(0);

    /// Counts calls but never frees, so a test can pass a deliberately wrong
    /// `len` (e.g. the oversized case) without triggering an invalid free.
    extern "C" fn count_drop_decode(_ptr: *mut u8, _len: usize) {
        DROPS_DECODE.fetch_add(1, Ordering::SeqCst);
    }
    extern "C" fn count_drop_oversized(_ptr: *mut u8, _len: usize) {
        DROPS_OVERSIZED.fetch_add(1, Ordering::SeqCst);
    }

    fn encoded_source_event() -> Vec<u8> {
        let wrapper = SourceEventWrapper {
            source_id: "src-1".to_string(),
            event: SourceEvent::Change(SourceChange::Insert {
                element: sample_node(),
            }),
            timestamp: Utc::now(),
            profiling: None,
            sequence: Some(1),
            source_position: None,
        };
        let payload = SourceEventPayload::from_wrapper(&wrapper);
        rmp_serde::to_vec_named(&payload).unwrap()
    }

    #[test]
    fn take_ffi_payload_decodes_and_calls_drop() {
        let bytes = encoded_source_event();
        let len = bytes.len();
        let ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;

        let decoded = unsafe {
            take_ffi_payload(
                ptr,
                len,
                Some(count_drop_decode),
                decode_source_event_payload,
            )
        };
        assert!(decoded.is_some());
        assert_eq!(DROPS_DECODE.load(Ordering::SeqCst), 1);

        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                ptr as *mut u8,
                len,
            )))
        };
    }

    #[test]
    fn take_ffi_payload_rejects_oversized_without_reading() {
        // A real (small) buffer, but we lie about its length to exceed the cap.
        // `take_ffi_payload` must reject it *before* slicing/decoding (no OOB read),
        // and must still invoke the producer's drop fn.
        let bytes = vec![0u8; 16];
        let real_len = bytes.len();
        let ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;

        let decoded = unsafe {
            take_ffi_payload(
                ptr,
                MAX_FFI_PAYLOAD_BYTES + 1,
                Some(count_drop_oversized),
                decode_source_event_payload,
            )
        };
        assert!(decoded.is_none(), "oversized payload must be rejected");
        assert_eq!(
            DROPS_OVERSIZED.load(Ordering::SeqCst),
            1,
            "buffer must still be freed"
        );

        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                ptr as *mut u8,
                real_len,
            )))
        };
    }

    #[test]
    fn take_ffi_payload_null_drop_fn_does_not_crash() {
        // A buggy/malicious producer setting `drop_fn = None` (null) must not crash
        // the process (no panic recovery across `extern "C"`). The buffer leaks, but
        // the consumer stays alive and still decodes the event.
        let bytes = encoded_source_event();
        let len = bytes.len();
        let ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;

        let decoded = unsafe { take_ffi_payload(ptr, len, None, decode_source_event_payload) };
        assert!(
            decoded.is_some(),
            "decode should still succeed; only the free is skipped"
        );

        // Buffer was leaked (drop fn was null) — reclaim it so the test doesn't leak.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                ptr as *mut u8,
                len,
            )))
        };
    }

    // ---- consume_query_result ownership (host→plugin path) ----

    // Dedicated per-test counters (avoid shared-static / reset races under
    // parallel test execution).
    static QR_DROPS_VALID: AtomicUsize = AtomicUsize::new(0);
    static QR_DROPS_INVALID: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn count_drop_qr_valid(ptr: *mut u8, len: usize) {
        QR_DROPS_VALID.fetch_add(1, Ordering::SeqCst);
        free_raw_bytes(ptr, len);
    }
    extern "C" fn count_drop_qr_invalid(ptr: *mut u8, len: usize) {
        QR_DROPS_INVALID.fetch_add(1, Ordering::SeqCst);
        free_raw_bytes(ptr, len);
    }

    fn free_raw_bytes(ptr: *mut u8, len: usize) {
        if !ptr.is_null() && len > 0 {
            unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len))) };
        }
    }

    /// Mirror the host producer: build a `*mut FfiQueryResult` from a serialized
    /// payload buffer guarded by a counting `payload_drop_fn`.
    fn make_ffi_query_result(
        bytes: Vec<u8>,
        drop_fn: extern "C" fn(*mut u8, usize),
    ) -> *mut FfiQueryResult {
        let payload_len = bytes.len();
        let payload_ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;
        Box::into_raw(Box::new(FfiQueryResult {
            payload_ptr,
            payload_len,
            payload_drop_fn: Some(drop_fn),
        }))
    }

    #[test]
    fn consume_query_result_decodes_and_frees_buffer_once() {
        let qr = drasi_lib::channels::QueryResult::new(
            "q-1".to_string(),
            7,
            Utc::now(),
            Vec::new(),
            std::collections::HashMap::new(),
        );
        let raw = make_ffi_query_result(encode_query_result(&qr), count_drop_qr_valid);

        let decoded = unsafe { consume_query_result(raw) }.expect("owned QueryResult");
        assert_eq!(decoded.query_id, "q-1");
        assert_eq!(decoded.sequence, 7);

        // The host's payload buffer was freed exactly once (no leak, no double-free),
        // and the envelope `Box` was reclaimed by `consume_query_result`.
        assert_eq!(QR_DROPS_VALID.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn consume_query_result_handles_undecodable_payload_without_leak() {
        // Garbage bytes that are not a valid QueryResult.
        let raw = make_ffi_query_result(vec![0xFFu8; 8], count_drop_qr_invalid);

        let decoded = unsafe { consume_query_result(raw) };
        assert!(decoded.is_none(), "garbage payload must not decode");
        // Buffer still freed exactly once even though decode failed.
        assert_eq!(QR_DROPS_INVALID.load(Ordering::SeqCst), 1);
    }
}
