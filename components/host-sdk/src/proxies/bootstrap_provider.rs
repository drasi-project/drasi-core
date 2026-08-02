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

//! Host-side proxy for BootstrapProvider and BootstrapPluginDescriptor.

use std::ffi::c_void;
use std::sync::Arc;

use async_trait::async_trait;

use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::events::BootstrapEventSender;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_plugin_sdk::descriptor::BootstrapPluginDescriptor;
use drasi_plugin_sdk::ffi::{
    release_bootstrap_receiver, release_result_receiver, wrap_result_receiver,
    BootstrapPluginVtable, BootstrapProviderVtable, BootstrapStreamConsumer, FfiStr,
};
use libloading::Library;

/// Wraps a `BootstrapProviderVtable` into a DrasiLib `BootstrapProvider`.
///
/// The host creates this when bridging bootstrap providers across plugin boundaries
/// (e.g., a bootstrap plugin providing data to a source plugin).
pub struct BootstrapProviderProxy {
    vtable: BootstrapProviderVtable,
    _library: Option<Arc<Library>>,
}

unsafe impl Send for BootstrapProviderProxy {}
unsafe impl Sync for BootstrapProviderProxy {}

impl BootstrapProviderProxy {
    pub fn new(vtable: BootstrapProviderVtable, library: Option<Arc<Library>>) -> Self {
        Self {
            vtable,
            _library: library,
        }
    }
}

#[async_trait]
impl BootstrapProvider for BootstrapProviderProxy {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        // Start the provider and take ownership of the stream handles inside a
        // block so no raw pointer is held across an await point. The guard
        // keeps the result callback context alive until the result is read.
        let (consumer, (result_rx, _result_guard)) = {
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

            // Non-blocking: spawns the provider and returns push-based handles.
            let stream_ptr = (self.vtable.bootstrap_fn)(
                self.vtable.state,
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
                return Err(anyhow::anyhow!(
                    "Bootstrap provider failed to start (null stream)"
                ));
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
                return Err(anyhow::anyhow!(
                    "Bootstrap provider returned an incomplete stream"
                ));
            }
            let events = unsafe { *Box::from_raw(stream.events) };
            let result = unsafe { *Box::from_raw(stream.result) };

            // Start the consumers on a dedicated thread to avoid initializing
            // plugin TLS on the caller's thread (same rationale as the
            // receiver proxies in SourceProxy::subscribe: on macOS, plugin TLS
            // destructors can deadlock with the still-running plugin runtime
            // during thread exit).
            std::thread::spawn(move || {
                (
                    BootstrapStreamConsumer::new(events),
                    wrap_result_receiver(result),
                )
            })
            .join()
            .map_err(|_| anyhow::anyhow!("Bootstrap consumer construction thread panicked"))?
        };

        // Drain every event into the query-side channel, then collect the
        // provider's result. The consumer returns only after the producer's
        // end-of-stream sentinel, so all events precede the result; dropping
        // event_tx on return closes the query's bootstrap stream. Consumer
        // backpressure propagates all the way to the provider: nothing on this
        // path buffers without bound and nothing blocks an async worker.
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

impl Drop for BootstrapProviderProxy {
    fn drop(&mut self) {
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        super::drop_worker::execute_drop_fn(drop_fn, state);
    }
}

// ============================================================================
// BootstrapPluginProxy — wraps BootstrapPluginVtable → BootstrapPluginDescriptor
// ============================================================================

/// Wraps a `BootstrapPluginVtable` (factory) into a `BootstrapPluginDescriptor`.
pub struct BootstrapPluginProxy {
    vtable: BootstrapPluginVtable,
    library: Arc<Library>,
    cached_kind: String,
    cached_config_version: String,
    cached_config_schema_name: String,
    plugin_id: String,
}

unsafe impl Send for BootstrapPluginProxy {}
unsafe impl Sync for BootstrapPluginProxy {}

impl BootstrapPluginProxy {
    pub fn new(vtable: BootstrapPluginVtable, library: Arc<Library>) -> Self {
        let cached_kind = unsafe { (vtable.kind_fn)(vtable.state as *const c_void).to_string() };
        let cached_config_version =
            unsafe { (vtable.config_version_fn)(vtable.state as *const c_void).to_string() };
        let cached_config_schema_name =
            unsafe { (vtable.config_schema_name_fn)(vtable.state as *const c_void).to_string() };
        Self {
            vtable,
            library,
            cached_kind,
            cached_config_version,
            cached_config_schema_name,
            plugin_id: String::new(),
        }
    }

    /// The unique identifier of the plugin that provided this descriptor.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Set the plugin identity for this descriptor.
    pub fn set_plugin_id(&mut self, id: String) {
        self.plugin_id = id;
    }
}

#[async_trait]
impl BootstrapPluginDescriptor for BootstrapPluginProxy {
    fn kind(&self) -> &str {
        &self.cached_kind
    }

    fn config_version(&self) -> &str {
        &self.cached_config_version
    }

    fn config_schema_json(&self) -> String {
        unsafe {
            (self.vtable.config_schema_json_fn)(self.vtable.state as *const c_void).into_string()
        }
    }

    fn config_schema_name(&self) -> &str {
        &self.cached_config_schema_name
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let config_str = serde_json::to_string(config_json)?;
        let source_config_str = serde_json::to_string(source_config_json)?;
        let config_ffi = FfiStr::from_str(&config_str);
        let source_config_ffi = FfiStr::from_str(&source_config_str);

        let state = self.vtable.state;
        let create_fn = self.vtable.create_bootstrap_provider_fn;
        let result = (create_fn)(state, config_ffi, source_config_ffi);

        let vtable_ptr = unsafe {
            result
                .into_result::<BootstrapProviderVtable>()
                .map_err(|msg| anyhow::anyhow!("{msg}"))?
        };

        if vtable_ptr.is_null() {
            return Err(anyhow::anyhow!(
                "Plugin factory returned null for bootstrap provider"
            ));
        }

        let vtable = unsafe { *Box::from_raw(vtable_ptr) };
        Ok(Box::new(BootstrapProviderProxy::new(
            vtable,
            Some(self.library.clone()),
        )))
    }
}

impl Drop for BootstrapPluginProxy {
    fn drop(&mut self) {
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        super::drop_worker::execute_drop_fn(drop_fn, state);
    }
}
