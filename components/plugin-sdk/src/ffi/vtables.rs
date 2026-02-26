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

//! `#[repr(C)]` vtable structs for the FFI boundary.
//!
//! These structs are the "contracts" between host and plugin. Each vtable
//! holds a `state` pointer (the opaque `self`) plus function pointers that
//! the caller dispatches through.

use std::ffi::c_void;

use super::types::{
    AsyncExecutorFn, FfiChangeOp, FfiComponentStatus, FfiDispatchMode, FfiGetResult, FfiOwnedStr,
    FfiResult, FfiStr, FfiStringArray,
};

// ============================================================================
// Source events — carries SourceChange / SourceEventWrapper across FFI
// ============================================================================

/// Represents a source change event that crosses the FFI boundary.
/// The opaque pointer holds a heap-allocated Rust type (e.g., `SourceEventWrapper`)
/// that the plugin owns. The FFI metadata fields expose key information so the
/// host can route/filter without deserializing.
#[repr(C)]
pub struct FfiSourceEvent {
    pub opaque: *mut c_void,
    pub source_id: FfiStr,
    pub timestamp_us: i64,
    pub op: FfiChangeOp,
    pub label: FfiStr,
    pub entity_id: FfiStr,
    pub drop_fn: extern "C" fn(*mut c_void),
}

/// Bootstrap event — initial state data loaded before streaming begins.
#[repr(C)]
pub struct FfiBootstrapEvent {
    pub opaque: *mut c_void,
    pub source_id: FfiStr,
    pub timestamp_us: i64,
    pub sequence: u64,
    pub label: FfiStr,
    pub entity_id: FfiStr,
    pub drop_fn: extern "C" fn(*mut c_void),
}

// ============================================================================
// Receivers — streaming events from plugin to host
// ============================================================================

/// Change receiver — wraps a `Box<dyn ChangeReceiver<SourceEventWrapper>>`.
#[repr(C)]
pub struct FfiChangeReceiver {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,
    pub recv_fn: extern "C" fn(state: *mut c_void) -> *mut FfiSourceEvent,
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for FfiChangeReceiver {}
unsafe impl Sync for FfiChangeReceiver {}

/// Bootstrap receiver — finite stream of bootstrap events.
#[repr(C)]
pub struct FfiBootstrapReceiver {
    pub state: *mut c_void,
    pub recv_fn: extern "C" fn(state: *mut c_void) -> *mut FfiBootstrapEvent,
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for FfiBootstrapReceiver {}
unsafe impl Sync for FfiBootstrapReceiver {}

/// Subscription response — streaming receiver + optional bootstrap receiver.
#[repr(C)]
pub struct FfiSubscriptionResponse {
    pub query_id: FfiOwnedStr,
    pub source_id: FfiOwnedStr,
    pub receiver: *mut FfiChangeReceiver,
    /// Null if no bootstrap data available.
    pub bootstrap_receiver: *mut FfiBootstrapReceiver,
}

// ============================================================================
// Query result events — carries QueryResult across FFI to reactions
// ============================================================================

/// Represents a query result event delivered to reactions.
/// The opaque pointer holds a concrete `QueryResult`.
#[repr(C)]
pub struct FfiQueryResultEvent {
    pub opaque: *mut c_void,
    pub query_id: FfiStr,
    pub timestamp_us: i64,
    pub sequence: u64,
    pub result_count: usize,
    pub drop_fn: extern "C" fn(*mut c_void),
}

/// Receiver for query result events (reaction-side).
#[repr(C)]
pub struct FfiQueryResultReceiver {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,
    pub recv_fn: extern "C" fn(state: *mut c_void) -> *mut FfiQueryResultEvent,
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for FfiQueryResultReceiver {}
unsafe impl Sync for FfiQueryResultReceiver {}

// ============================================================================
// Bootstrap sender — callback for sending bootstrap records from provider
// ============================================================================

/// FFI-safe callback for sending bootstrap records from a BootstrapProvider.
/// The host creates this and passes it to the provider.
#[repr(C)]
pub struct FfiBootstrapSender {
    pub state: *mut c_void,
    /// Send a bootstrap record. Returns 0 on success, non-zero if channel closed.
    pub send_fn: extern "C" fn(state: *mut c_void, event: *mut FfiBootstrapEvent) -> i32,
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for FfiBootstrapSender {}
unsafe impl Sync for FfiBootstrapSender {}

// ============================================================================
// Runtime context — host services provided to plugins during initialization
// ============================================================================

/// Runtime context passed from host to plugin during initialization.
/// Contains vtable-based callbacks for host services — no trait objects.
#[repr(C)]
pub struct FfiRuntimeContext {
    pub instance_id: FfiStr,
    pub component_id: FfiStr,
    /// Nullable — not all plugins need state store.
    pub state_store: *const StateStoreVtable,
    /// Per-instance log callback (nullable — falls back to global if null).
    pub log_callback: Option<super::callbacks::LogCallbackFn>,
    /// Opaque context for per-instance log callback.
    pub log_ctx: *mut c_void,
    /// Per-instance lifecycle callback (nullable — falls back to global if null).
    pub lifecycle_callback: Option<super::callbacks::LifecycleCallbackFn>,
    /// Opaque context for per-instance lifecycle callback.
    pub lifecycle_ctx: *mut c_void,
    /// Nullable — identity provider for credential injection.
    pub identity_provider: *const super::identity::IdentityProviderVtable,
}

// ============================================================================
// Source vtable
// ============================================================================

/// FFI-safe vtable for a Source instance.
#[repr(C)]
pub struct SourceVtable {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,

    // Identity
    pub id_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub type_name_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub auto_start_fn: extern "C" fn(state: *const c_void) -> bool,
    pub dispatch_mode_fn: extern "C" fn(state: *const c_void) -> FfiDispatchMode,

    // Lifecycle
    pub start_fn: extern "C" fn(state: *mut c_void) -> FfiResult,
    pub stop_fn: extern "C" fn(state: *mut c_void) -> FfiResult,
    pub status_fn: extern "C" fn(state: *const c_void) -> FfiComponentStatus,
    pub deprovision_fn: extern "C" fn(state: *mut c_void) -> FfiResult,

    // Initialization
    pub initialize_fn: extern "C" fn(state: *mut c_void, ctx: *const FfiRuntimeContext),

    // Subscriptions
    /// Subscribe with query_id, node_labels JSON, relation_labels JSON.
    pub subscribe_fn: extern "C" fn(
        state: *mut c_void,
        source_id: FfiStr,
        enable_bootstrap: bool,
        query_id: FfiStr,
        nodes_json: FfiStr,
        relations_json: FfiStr,
    ) -> *mut FfiSubscriptionResponse,

    /// Host calls this to inject an external bootstrap provider (from another plugin).
    /// The vtable pointer is owned by the source after this call.
    pub set_bootstrap_provider_fn:
        extern "C" fn(state: *mut c_void, provider: *mut BootstrapProviderVtable),

    // Cleanup
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for SourceVtable {}
unsafe impl Sync for SourceVtable {}

// ============================================================================
// Reaction vtable
// ============================================================================

/// FFI-safe vtable for a Reaction instance.
#[repr(C)]
pub struct ReactionVtable {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,

    // Identity
    pub id_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub type_name_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub auto_start_fn: extern "C" fn(state: *const c_void) -> bool,
    pub query_ids_fn: extern "C" fn(state: *const c_void) -> FfiStringArray,

    // Lifecycle
    pub start_fn: extern "C" fn(state: *mut c_void) -> FfiResult,
    pub stop_fn: extern "C" fn(state: *mut c_void) -> FfiResult,
    pub status_fn: extern "C" fn(state: *const c_void) -> FfiComponentStatus,
    pub deprovision_fn: extern "C" fn(state: *mut c_void) -> FfiResult,

    // Initialization
    pub initialize_fn: extern "C" fn(state: *mut c_void, ctx: *const FfiRuntimeContext),

    // Cleanup
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for ReactionVtable {}
unsafe impl Sync for ReactionVtable {}

// ============================================================================
// Bootstrap provider vtable
// ============================================================================

/// FFI-safe vtable for a BootstrapProvider.
/// The bootstrap plugin creates this; the host wraps it and passes it to the source plugin.
#[repr(C)]
pub struct BootstrapProviderVtable {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,
    /// Perform bootstrap. Sends records via the FfiBootstrapSender.
    /// Returns the count of records sent (>= 0), or negative on error.
    ///
    /// Parameters:
    /// - `query_id` — the subscribing query's ID
    /// - `node_labels` / `node_labels_count` — node labels to bootstrap
    /// - `relation_labels` / `relation_labels_count` — relation labels to bootstrap
    /// - `request_id` — unique request ID for this bootstrap operation
    /// - `server_id` — the server/instance ID
    /// - `source_id` — the source ID this bootstrap is for
    /// - `sender` — FFI sender for bootstrap events
    pub bootstrap_fn: extern "C" fn(
        state: *mut c_void,
        query_id: FfiStr,
        node_labels: *const FfiStr,
        node_labels_count: usize,
        relation_labels: *const FfiStr,
        relation_labels_count: usize,
        request_id: FfiStr,
        server_id: FfiStr,
        source_id: FfiStr,
        sender: *mut FfiBootstrapSender,
    ) -> i64,
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for BootstrapProviderVtable {}
unsafe impl Sync for BootstrapProviderVtable {}

// ============================================================================
// Plugin descriptor vtables — factories that create Source/Reaction instances
// ============================================================================

/// FFI-safe vtable for a SourcePluginDescriptor (factory).
/// The host calls `create_source_fn` to construct a SourceVtable from config.
#[repr(C)]
pub struct SourcePluginVtable {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,

    pub kind_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub config_version_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub config_schema_json_fn: extern "C" fn(state: *const c_void) -> FfiOwnedStr,
    pub config_schema_name_fn: extern "C" fn(state: *const c_void) -> FfiStr,

    pub create_source_fn: extern "C" fn(
        state: *mut c_void,
        id: FfiStr,
        config_json: FfiStr,
        auto_start: bool,
    ) -> *mut SourceVtable,

    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for SourcePluginVtable {}
unsafe impl Sync for SourcePluginVtable {}

/// FFI-safe vtable for a ReactionPluginDescriptor (factory).
#[repr(C)]
pub struct ReactionPluginVtable {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,

    pub kind_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub config_version_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub config_schema_json_fn: extern "C" fn(state: *const c_void) -> FfiOwnedStr,
    pub config_schema_name_fn: extern "C" fn(state: *const c_void) -> FfiStr,

    /// Factory: create a ReactionVtable from JSON config.
    /// `query_ids_json` — JSON array of query IDs.
    pub create_reaction_fn: extern "C" fn(
        state: *mut c_void,
        id: FfiStr,
        query_ids_json: FfiStr,
        config_json: FfiStr,
        auto_start: bool,
    ) -> *mut ReactionVtable,

    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for ReactionPluginVtable {}
unsafe impl Sync for ReactionPluginVtable {}

/// FFI-safe vtable for a BootstrapPluginDescriptor (factory).
#[repr(C)]
pub struct BootstrapPluginVtable {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,

    pub kind_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub config_version_fn: extern "C" fn(state: *const c_void) -> FfiStr,
    pub config_schema_json_fn: extern "C" fn(state: *const c_void) -> FfiOwnedStr,
    pub config_schema_name_fn: extern "C" fn(state: *const c_void) -> FfiStr,

    /// Factory: create a BootstrapProviderVtable from JSON config.
    /// Takes both the bootstrap-specific config and the parent source config.
    pub create_bootstrap_provider_fn: extern "C" fn(
        state: *mut c_void,
        config_json: FfiStr,
        source_config_json: FfiStr,
    ) -> *mut BootstrapProviderVtable,

    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for BootstrapPluginVtable {}
unsafe impl Sync for BootstrapPluginVtable {}

// ============================================================================
// State store vtable — reverse direction (host → plugin)
// ============================================================================

/// State store vtable — host creates and provides to plugins.
/// Plugins call these function pointers to access persistent state.
#[repr(C)]
pub struct StateStoreVtable {
    pub state: *mut c_void,
    // Basic operations
    pub get_fn:
        extern "C" fn(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiGetResult,
    pub set_fn: extern "C" fn(
        state: *mut c_void,
        store_id: FfiStr,
        key: FfiStr,
        value: *const u8,
        value_len: usize,
    ) -> FfiResult,
    pub delete_fn:
        extern "C" fn(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiResult,
    pub contains_key_fn:
        extern "C" fn(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiResult,
    // Batch operations (keys passed as FfiStr arrays)
    pub get_many_fn: extern "C" fn(
        state: *mut c_void,
        store_id: FfiStr,
        keys: *const FfiStr,
        keys_count: usize,
        out_values: *mut FfiGetResult,
    ) -> FfiResult,
    pub set_many_fn: extern "C" fn(
        state: *mut c_void,
        store_id: FfiStr,
        keys: *const FfiStr,
        values: *const *const u8,
        value_lens: *const usize,
        count: usize,
    ) -> FfiResult,
    pub delete_many_fn: extern "C" fn(
        state: *mut c_void,
        store_id: FfiStr,
        keys: *const FfiStr,
        keys_count: usize,
    ) -> i64,
    // Store-level operations
    pub clear_store_fn: extern "C" fn(state: *mut c_void, store_id: FfiStr) -> i64,
    pub list_keys_fn:
        extern "C" fn(state: *mut c_void, store_id: FfiStr) -> FfiStringArray,
    pub store_exists_fn:
        extern "C" fn(state: *mut c_void, store_id: FfiStr) -> FfiResult,
    pub key_count_fn: extern "C" fn(state: *mut c_void, store_id: FfiStr) -> i64,
    pub sync_fn: extern "C" fn(state: *mut c_void) -> FfiResult,
    // Cleanup
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for StateStoreVtable {}
unsafe impl Sync for StateStoreVtable {}

// ============================================================================
// Plugin registration — returned by drasi_plugin_init() for cdylib builds
// ============================================================================

/// FFI-safe plugin registration returned by `drasi_plugin_init()`.
/// Contains factory vtables for all plugin types this shared library provides.
#[repr(C)]
pub struct FfiPluginRegistration {
    pub source_plugins: *mut SourcePluginVtable,
    pub source_plugin_count: usize,
    pub reaction_plugins: *mut ReactionPluginVtable,
    pub reaction_plugin_count: usize,
    pub bootstrap_plugins: *mut BootstrapPluginVtable,
    pub bootstrap_plugin_count: usize,
    /// Host calls this to provide a log callback with an opaque context pointer.
    /// The plugin stores both the callback and context, passing context back on every call.
    pub set_log_callback:
        extern "C" fn(ctx: *mut ::std::ffi::c_void, callback: super::callbacks::LogCallbackFn),
    /// Host calls this to provide a lifecycle event callback with an opaque context pointer.
    pub set_lifecycle_callback:
        extern "C" fn(ctx: *mut ::std::ffi::c_void, callback: super::callbacks::LifecycleCallbackFn),
}
