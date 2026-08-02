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

//! Plugin-side bootstrap provider proxy.
//!
//! Wraps a `BootstrapProviderVtable` (from another cdylib plugin or the host)
//! into a local `BootstrapProvider` trait implementation. This is how a source
//! plugin calls a bootstrap provider that lives in a different cdylib.
//!
//! The vtable's `bootstrap_fn` returns push-based receivers immediately; this
//! proxy consumes them via [`BootstrapStreamConsumer`] and
//! [`wrap_result_receiver`], forwarding events into the caller's bounded
//! channel. Backpressure propagates through every link (see the
//! `bootstrap_stream` module docs); nothing blocks an async worker.

use std::sync::Mutex;

use super::bootstrap_stream::{
    release_bootstrap_receiver, release_result_receiver, wrap_result_receiver,
    BootstrapStreamConsumer,
};
use super::types::FfiStr;
use super::vtables::BootstrapProviderVtable;
use drasi_lib::bootstrap::{BootstrapProvider, BootstrapResult};

/// Plugin-side proxy: wraps a `BootstrapProviderVtable` into a local `BootstrapProvider`.
pub struct FfiBootstrapProviderProxy {
    pub(crate) vtable: Mutex<BootstrapProviderVtable>,
}

unsafe impl Send for FfiBootstrapProviderProxy {}
unsafe impl Sync for FfiBootstrapProviderProxy {}

impl FfiBootstrapProviderProxy {
    /// Wrap a provider vtable received across the FFI boundary.
    pub fn new(vtable: BootstrapProviderVtable) -> Self {
        Self {
            vtable: Mutex::new(vtable),
        }
    }
}

#[async_trait::async_trait]
impl BootstrapProvider for FfiBootstrapProviderProxy {
    async fn bootstrap(
        &self,
        request: drasi_lib::bootstrap::BootstrapRequest,
        context: &drasi_lib::bootstrap::BootstrapContext,
        event_tx: drasi_lib::channels::events::BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        // Start the provider and take ownership of the stream handles inside a
        // block so no raw pointer is held across an await point.
        //
        // Unlike host-sdk's BootstrapProviderProxy, the consumers are built
        // inline rather than on a dedicated thread: the vtable functions here
        // are host-compiled, only spawn threads and return, and this caller is
        // a long-lived plugin runtime worker — so the macOS TLS-destructor-at-
        // thread-exit hazard that motivates the dedicated thread over there
        // does not apply in this direction.
        // The guard keeps the result callback context alive until the result
        // is read.
        let (consumer, (result_rx, _result_guard)) = {
            let (vtable_state, vtable_bootstrap_fn) = {
                let vtable = self.vtable.lock().expect("vtable mutex poisoned");
                (vtable.state, vtable.bootstrap_fn)
            };

            let node_ffi: Vec<FfiStr> = request
                .node_labels
                .iter()
                .map(|s| FfiStr::from_str(s))
                .collect();
            let rel_ffi: Vec<FfiStr> = request
                .relation_labels
                .iter()
                .map(|s| FfiStr::from_str(s))
                .collect();

            // Non-blocking: spawns the provider and returns handles.
            let stream_ptr = (vtable_bootstrap_fn)(
                vtable_state,
                FfiStr::from_str(&request.query_id),
                node_ffi.as_ptr(),
                node_ffi.len(),
                rel_ffi.as_ptr(),
                rel_ffi.len(),
                FfiStr::from_str(&request.request_id),
                FfiStr::from_str(&context.server_id),
                FfiStr::from_str(&context.source_id),
            );

            if stream_ptr.is_null() {
                anyhow::bail!("Bootstrap provider failed to start (null stream)");
            }
            let stream = unsafe { *Box::from_raw(stream_ptr) };
            if stream.events.is_null() || stream.result.is_null() {
                // Release whichever receiver was populated so neither its
                // state nor the provider thread blocked on it leaks.
                if !stream.events.is_null() {
                    release_bootstrap_receiver(unsafe { *Box::from_raw(stream.events) });
                }
                if !stream.result.is_null() {
                    release_result_receiver(unsafe { *Box::from_raw(stream.result) });
                }
                anyhow::bail!("Bootstrap provider returned an incomplete stream");
            }
            let events = unsafe { *Box::from_raw(stream.events) };
            let result = unsafe { *Box::from_raw(stream.result) };

            (
                BootstrapStreamConsumer::new(events),
                wrap_result_receiver(result),
            )
        };

        // Drain every event into the caller's channel, then collect the
        // provider's result. The consumer returns only after the producer's
        // end-of-stream sentinel, so all events precede the result.
        let forwarded = consumer.forward_into(&event_tx).await;
        let outcome = result_rx
            .await
            .map_err(|_| anyhow::anyhow!("Bootstrap result channel dropped without a result"))?;
        let result = outcome?;

        if result.event_count != forwarded {
            log::warn!(
                "Bootstrap event count mismatch: provider reported {} but {forwarded} events \
                 were delivered",
                result.event_count
            );
        }
        log::debug!(
            "FFI bootstrap stream complete: {forwarded} events forwarded, provider reported {}",
            result.event_count
        );
        Ok(result)
    }
}
