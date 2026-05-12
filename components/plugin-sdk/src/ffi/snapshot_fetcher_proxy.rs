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

//! Plugin-side snapshot fetcher proxy that wraps a `SnapshotFetcherVtable` into
//! a `SnapshotFetcher` trait implementation.
//!
//! The host provides a `SnapshotFetcherVtable` (function pointers backed by its own
//! `Arc<dyn SnapshotFetcher>`). The plugin wraps it in `FfiSnapshotFetcherProxy`
//! and uses it as a normal `SnapshotFetcher`.

use super::types::{FfiStr, SendMutPtr};
use super::vtable_gen::{make_snapshot_stream, SendFfiSnapshotIteratorResponse};
use super::vtables::SnapshotFetcherVtable;

/// Plugin-side proxy: wraps a host-provided `SnapshotFetcherVtable` into a local
/// `SnapshotFetcher` implementation.
pub struct FfiSnapshotFetcherProxy {
    pub(crate) vtable: *const SnapshotFetcherVtable,
}

unsafe impl Send for FfiSnapshotFetcherProxy {}
unsafe impl Sync for FfiSnapshotFetcherProxy {}

#[async_trait::async_trait]
impl drasi_lib::SnapshotFetcher for FfiSnapshotFetcherProxy {
    async fn fetch_snapshot(
        &self,
        query_id: &str,
    ) -> Result<
        drasi_lib::queries::output_state::SnapshotStream,
        drasi_lib::queries::output_state::FetchError,
    > {
        let vtable = unsafe { &*self.vtable };
        let cb = vtable.fetch_snapshot_fn;
        let state = SendMutPtr(vtable.state);
        let query_id_owned = query_id.to_string();

        // Call the host callback on a blocking thread to avoid blocking async workers
        let wrapped = tokio::task::spawn_blocking(move || {
            let query_id_ffi = FfiStr::from_str(&query_id_owned);
            SendFfiSnapshotIteratorResponse(cb(state.as_ptr(), query_id_ffi))
        })
        .await
        .map_err(
            |_| drasi_lib::queries::output_state::FetchError::NotRunning {
                status: drasi_lib::ComponentStatus::Error,
            },
        )?;

        let resp = wrapped.0;

        // Consume error string unconditionally to prevent leaks.
        let err_str = unsafe { resp.error.into_string() };
        if !err_str.is_empty() {
            log::error!("[FFI fetch_snapshot] host returned error: {err_str}");
            return Err(drasi_lib::queries::output_state::FetchError::NotRunning {
                status: drasi_lib::ComponentStatus::Error,
            });
        }

        let as_of_sequence = resp.as_of_sequence;
        let config_hash = resp.config_hash;

        // Wrap the FFI iterator in an async stream (reuses bootstrap stream wrapper)
        let stream = make_snapshot_stream(resp.iterator);

        Ok(
            drasi_lib::queries::output_state::SnapshotStream::from_stream(
                stream,
                as_of_sequence,
                config_hash,
            ),
        )
    }
}
