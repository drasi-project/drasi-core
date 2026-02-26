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

use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::events::{BootstrapEvent, BootstrapEventSender};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_plugin_sdk::descriptor::BootstrapPluginDescriptor;
use drasi_plugin_sdk::ffi::{
    BootstrapPluginVtable, BootstrapProviderVtable, FfiBootstrapEvent, FfiBootstrapSender, FfiStr,
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
    ) -> anyhow::Result<usize> {
        // Build FfiStr arrays for node/relation labels
        let node_label_strs: Vec<String> = request.node_labels.clone();
        let rel_label_strs: Vec<String> = request.relation_labels.clone();
        let node_ffi: Vec<FfiStr> = node_label_strs.iter().map(|s| FfiStr::from_str(s)).collect();
        let rel_ffi: Vec<FfiStr> = rel_label_strs.iter().map(|s| FfiStr::from_str(s)).collect();

        let query_id_ffi = FfiStr::from_str(&request.query_id);
        let request_id_ffi = FfiStr::from_str(&request.request_id);
        let server_id_ffi = FfiStr::from_str(&context.server_id);
        let source_id_ffi = FfiStr::from_str(&context.source_id);

        // Build the FfiBootstrapSender that forwards events into the tokio channel
        let sender = build_ffi_bootstrap_sender(event_tx);
        let sender_ptr = Box::into_raw(Box::new(sender));

        let state = self.vtable.state;
        let bootstrap_fn = self.vtable.bootstrap_fn;
        let node_labels_ptr = node_ffi.as_ptr();
        let node_labels_count = node_ffi.len();
        let rel_labels_ptr = rel_ffi.as_ptr();
        let rel_labels_count = rel_ffi.len();

        let count = (bootstrap_fn)(
            state,
            query_id_ffi,
            node_labels_ptr,
            node_labels_count,
            rel_labels_ptr,
            rel_labels_count,
            request_id_ffi,
            server_id_ffi,
            source_id_ffi,
            sender_ptr,
        );

        if count < 0 {
            return Err(anyhow::anyhow!("Bootstrap failed with code {}", count));
        }

        Ok(count as usize)
    }
}

impl Drop for BootstrapProviderProxy {
    fn drop(&mut self) {
        (self.vtable.drop_fn)(self.vtable.state);
    }
}

/// Build an `FfiBootstrapSender` that forwards bootstrap events into a tokio mpsc sender.
///
/// Uses a std::sync::mpsc channel + forwarding thread to avoid blocking the plugin's
/// tokio runtime (the same pattern proven in the PoC's cross-plugin bootstrap).
fn build_ffi_bootstrap_sender(event_tx: BootstrapEventSender) -> FfiBootstrapSender {
    // Create a std::sync channel for the plugin to send on (sync-safe)
    let (std_tx, std_rx) = std::sync::mpsc::channel::<BootstrapEvent>();

    // Spawn a thread that forwards from std channel → tokio channel
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for event in std_rx {
            if rt.block_on(event_tx.send(event)).is_err() {
                break;
            }
        }
    });

    let state = Box::into_raw(Box::new(std_tx)) as *mut c_void;

    extern "C" fn send_fn(state: *mut c_void, event: *mut FfiBootstrapEvent) -> i32 {
        let tx = unsafe { &*(state as *const std::sync::mpsc::Sender<BootstrapEvent>) };
        if event.is_null() {
            return -1;
        }
        let ffi_event = unsafe { &*event };
        let bootstrap_event =
            unsafe { *Box::from_raw(ffi_event.opaque as *mut BootstrapEvent) };
        // Free the FFI envelope but not the opaque (we took ownership)
        unsafe { drop(Box::from_raw(event)) };
        match tx.send(bootstrap_event) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }

    extern "C" fn drop_fn(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut std::sync::mpsc::Sender<BootstrapEvent>)) };
    }

    FfiBootstrapSender {
        state,
        send_fn,
        drop_fn,
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
}

unsafe impl Send for BootstrapPluginProxy {}
unsafe impl Sync for BootstrapPluginProxy {}

impl BootstrapPluginProxy {
    pub fn new(vtable: BootstrapPluginVtable, library: Arc<Library>) -> Self {
        let cached_kind =
            unsafe { (vtable.kind_fn)(vtable.state as *const c_void).to_string() };
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
        }
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
        let vtable_ptr = (create_fn)(state, config_ffi, source_config_ffi);

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
        (self.vtable.drop_fn)(self.vtable.state);
    }
}
