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

use std::ffi::c_void;

use super::types::FfiStr;
use super::vtables::WalProviderVtable;
use drasi_core::models::SourceChange;
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

#[async_trait::async_trait]
impl WalProvider for FfiWalProviderProxy {
    async fn register(&self, source_id: &str, config: WriteAheadLogConfig) -> Result<(), WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let capacity_policy: u8 = match config.capacity_policy {
                CapacityPolicy::RejectIncoming => 0,
                CapacityPolicy::OverwriteOldest => 1,
            };
            let result = (vtable.register_fn)(
                vtable.state,
                FfiStr::from_str(source_id),
                config.max_events,
                capacity_policy,
            );
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn append(&self, source_id: &str, event: &SourceChange) -> Result<u64, WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let event_ptr = event as *const SourceChange as *const c_void;
            let result = (vtable.append_fn)(vtable.state, FfiStr::from_str(source_id), event_ptr);
            match result.into_result() {
                Ok(seq) => Ok(seq),
                Err((1, _msg)) => Err(WalError::CapacityExhausted(source_id.to_string())),
                Err((_code, msg)) => Err(WalError::StorageError(msg)),
            }
        }
    }

    async fn read_from(
        &self,
        source_id: &str,
        sequence: u64,
    ) -> Result<Vec<(u64, SourceChange)>, WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.read_from_fn)(vtable.state, FfiStr::from_str(source_id), sequence);

            if result.error_code != 0 {
                let msg = if !result.error_msg.is_null() {
                    std::ffi::CString::from_raw(result.error_msg)
                        .into_string()
                        .unwrap_or_default()
                } else {
                    "unknown WAL error".to_string()
                };
                return Err(WalError::StorageError(msg));
            }

            let mut events = Vec::with_capacity(result.count);
            if !result.entries.is_null() && result.count > 0 {
                let entries_slice = std::slice::from_raw_parts(result.entries, result.count);
                for entry in entries_slice {
                    // Take ownership of the SourceChange from the host
                    let source_change = *Box::from_raw(entry.event as *mut SourceChange);
                    events.push((entry.sequence, source_change));
                }
                // Free the entries buffer (NOT the SourceChange pointers, we took those)
                (vtable.free_read_result_fn)(result.entries, result.count, result.capacity);
            }

            Ok(events)
        }
    }

    async fn prune_up_to(&self, source_id: &str, sequence: u64) -> Result<u64, WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let result =
                (vtable.prune_up_to_fn)(vtable.state, FfiStr::from_str(source_id), sequence);
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn head_sequence(&self, source_id: &str) -> Result<u64, WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.head_sequence_fn)(vtable.state, FfiStr::from_str(source_id));
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn oldest_sequence(&self, source_id: &str) -> Result<Option<u64>, WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.oldest_sequence_fn)(vtable.state, FfiStr::from_str(source_id));
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn event_count(&self, source_id: &str) -> Result<u64, WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.event_count_fn)(vtable.state, FfiStr::from_str(source_id));
            result.into_result().map_err(WalError::StorageError)
        }
    }

    async fn delete_wal(&self, source_id: &str) -> Result<(), WalError> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.delete_wal_fn)(vtable.state, FfiStr::from_str(source_id));
            result.into_result().map_err(WalError::StorageError)
        }
    }
}
