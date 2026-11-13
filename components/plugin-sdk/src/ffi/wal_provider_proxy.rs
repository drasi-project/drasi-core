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

//! Plugin-side WAL provider proxy that wraps a `WalProviderVtable` into
//! a `WalProvider` trait implementation.
//!
//! The host provides a `WalProviderVtable` (function pointers backed by its own
//! `Arc<dyn WalProvider>`). The plugin wraps it in `FfiWalProviderProxy`
//! and uses it as a normal `WalProvider`.

use super::types::{FfiStr, FfiWalEntry, FfiWalReadResult};
use super::vtables::WalProviderVtable;
use drasi_core::models::SourceChange;
use drasi_core_types::{ComponentId, StorageScope};
use drasi_lib::wal::{CapacityPolicy, WalError, WalProvider, WriteAheadLogConfig};

/// Plugin-side proxy: wraps a host-provided `WalProviderVtable` into a local
/// `WalProvider` implementation.
pub struct FfiWalProviderProxy {
    pub(crate) vtable: *const WalProviderVtable,
}

unsafe impl Send for FfiWalProviderProxy {}
unsafe impl Sync for FfiWalProviderProxy {}

impl Drop for FfiWalProviderProxy {
    fn drop(&mut self) {
        unsafe {
            let vtable = &*self.vtable;
            // Free the inner state (Arc<WalBridgeState>)
            (vtable.drop_fn)(vtable.state);
            // Free the vtable struct itself (allocated via Box::into_raw in source.rs)
            let _ = Box::from_raw(self.vtable as *mut WalProviderVtable);
        }
    }
}

fn encode_scope(scope: &StorageScope) -> Result<String, WalError> {
    serde_json::to_string(scope).map_err(|error| WalError::StorageError(error.to_string()))
}

struct ReadResult {
    raw: FfiWalReadResult,
    release: extern "C" fn(*mut FfiWalEntry, usize, usize),
}

impl Drop for ReadResult {
    fn drop(&mut self) {
        if !self.raw.entries.is_null() {
            (self.release)(self.raw.entries, self.raw.count, self.raw.capacity);
        }
    }
}

#[async_trait::async_trait]
impl WalProvider for FfiWalProviderProxy {
    async fn register(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
        config: WriteAheadLogConfig,
    ) -> Result<(), WalError> {
        let scope_json = encode_scope(scope)?;
        unsafe {
            let vtable = &*self.vtable;
            let capacity_policy: u8 = match config.capacity_policy {
                CapacityPolicy::RejectIncoming => 0,
                CapacityPolicy::OverwriteOldest => 1,
            };
            let result = (vtable.register_fn)(
                vtable.state,
                FfiStr::from_str(&scope_json),
                FfiStr::from_str(source_id.as_str()),
                config.max_events,
                capacity_policy,
            );
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn append(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
        event: &SourceChange,
    ) -> Result<u64, WalError> {
        let scope_json = encode_scope(scope)?;
        let bytes = rmp_serde::to_vec_named(event).map_err(|error| {
            WalError::StorageError(format!("could not encode WAL event: {error}"))
        })?;
        if bytes.len() > super::payload::MAX_FFI_PAYLOAD_BYTES {
            return Err(WalError::StorageError(
                "WAL event exceeds the FFI payload limit".into(),
            ));
        }
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.append_fn)(
                vtable.state,
                FfiStr::from_str(&scope_json),
                FfiStr::from_str(source_id.as_str()),
                bytes.as_ptr(),
                bytes.len(),
            );
            match result.into_result() {
                Ok(seq) => Ok(seq),
                Err((1, _msg)) => Err(WalError::CapacityExhausted(source_id.clone())),
                Err((_code, msg)) => Err(WalError::StorageError(msg)),
            }
        }
    }

    async fn read_from(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
        sequence: u64,
    ) -> Result<Vec<(u64, SourceChange)>, WalError> {
        let scope_json = encode_scope(scope)?;
        unsafe {
            let vtable = &*self.vtable;
            let guard = ReadResult {
                raw: (vtable.read_from_fn)(
                    vtable.state,
                    FfiStr::from_str(&scope_json),
                    FfiStr::from_str(source_id.as_str()),
                    sequence,
                ),
                release: vtable.free_read_result_fn,
            };
            let result = &guard.raw;

            let message = if result.error_msg.is_null() {
                None
            } else {
                Some(
                    std::ffi::CString::from_raw(result.error_msg)
                        .to_string_lossy()
                        .into_owned(),
                )
            };
            if result.error_code != 0 {
                return Err(WalError::StorageError(message.unwrap_or_else(|| {
                    "WAL read failed without an error message".into()
                })));
            }
            if let Some(message) = message {
                return Err(WalError::StorageError(format!(
                    "WAL read returned contradictory success and error: {message}"
                )));
            }
            if result.count > result.capacity
                || result.count
                    > super::payload::MAX_FFI_PAYLOAD_BYTES / std::mem::size_of::<FfiWalEntry>()
                || result.count > 0 && (result.entries.is_null() || !result.entries.is_aligned())
            {
                return Err(WalError::StorageError(
                    "invalid WAL read-result buffer".into(),
                ));
            }

            let mut events = Vec::with_capacity(result.count);
            if !result.entries.is_null() && result.count > 0 {
                let entries_slice = std::slice::from_raw_parts(result.entries, result.count);
                for entry in entries_slice {
                    if entry.event.is_null()
                        || entry.event_len == 0
                        || entry.event_len > super::payload::MAX_FFI_PAYLOAD_BYTES
                    {
                        return Err(WalError::StorageError("invalid WAL event buffer".into()));
                    }
                    let source_change = rmp_serde::from_slice(std::slice::from_raw_parts(
                        entry.event,
                        entry.event_len,
                    ))
                    .map_err(|error| {
                        WalError::StorageError(format!("invalid WAL event payload: {error}"))
                    })?;
                    events.push((entry.sequence, source_change));
                }
            }

            Ok(events)
        }
    }

    async fn prune_up_to(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
        sequence: u64,
    ) -> Result<u64, WalError> {
        let scope_json = encode_scope(scope)?;
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.prune_up_to_fn)(
                vtable.state,
                FfiStr::from_str(&scope_json),
                FfiStr::from_str(source_id.as_str()),
                sequence,
            );
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn head_sequence(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
    ) -> Result<u64, WalError> {
        let scope_json = encode_scope(scope)?;
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.head_sequence_fn)(
                vtable.state,
                FfiStr::from_str(&scope_json),
                FfiStr::from_str(source_id.as_str()),
            );
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn oldest_sequence(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
    ) -> Result<Option<u64>, WalError> {
        let scope_json = encode_scope(scope)?;
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.oldest_sequence_fn)(
                vtable.state,
                FfiStr::from_str(&scope_json),
                FfiStr::from_str(source_id.as_str()),
            );
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn event_count(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
    ) -> Result<u64, WalError> {
        let scope_json = encode_scope(scope)?;
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.event_count_fn)(
                vtable.state,
                FfiStr::from_str(&scope_json),
                FfiStr::from_str(source_id.as_str()),
            );
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn delete_wal(
        &self,
        scope: &StorageScope,
        source_id: &ComponentId,
    ) -> Result<(), WalError> {
        let scope_json = encode_scope(scope)?;
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.delete_wal_fn)(
                vtable.state,
                FfiStr::from_str(&scope_json),
                FfiStr::from_str(source_id.as_str()),
            );
            result.into_result().map_err(WalError::StorageError)
        }
    }
}
