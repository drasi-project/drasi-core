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
    AsyncExecutorFn, FfiChangeOp, FfiComponentStatus, FfiCreateResult, FfiDispatchMode,
    FfiGetResult, FfiOwnedStr, FfiResult, FfiStr, FfiStringArray,
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

/// Callback function type for push-based change delivery.
/// Called by the plugin forwarder for each source event.
/// `ctx` is the host-owned context pointer, `event` is the event to deliver.
/// Returns `true` if the event was accepted, `false` to signal shutdown.
pub type FfiChangePushCallbackFn =
    extern "C" fn(ctx: *mut c_void, event: *mut FfiSourceEvent) -> bool;

/// Change receiver — push-based model.
///
/// Instead of the host polling via `recv_fn`, the host calls `start_push_fn`
/// with a callback. The plugin spawns a forwarder task that reads from the
/// underlying channel and invokes the callback for each event. This avoids
/// the `spawn_blocking` + `dispatch_to_runtime` round-trip per event.
#[repr(C)]
pub struct FfiChangeReceiver {
    pub state: *mut c_void,
    pub executor: AsyncExecutorFn,
    /// Start pushing events to the provided callback.
    /// The plugin spawns a forwarder task on its runtime.
    /// The callback is called once per event until the channel closes
    /// or the callback returns `false`.
    pub start_push_fn: extern "C" fn(
        state: *mut c_void,
        callback: FfiChangePushCallbackFn,
        callback_ctx: *mut c_void,
    ),
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for FfiChangeReceiver {}
unsafe impl Sync for FfiChangeReceiver {}

/// Callback function type for push-based bootstrap delivery.
/// Called by the plugin forwarder for each bootstrap event.
/// `ctx` is the host-owned context pointer, `event` is the event to deliver (null signals end-of-stream).
/// Returns `true` if the event was accepted, `false` to signal shutdown.
pub type FfiBootstrapPushCallbackFn =
    extern "C" fn(ctx: *mut c_void, event: *mut FfiBootstrapEvent) -> bool;

/// Bootstrap receiver — push-based finite stream of bootstrap events.
///
/// Like `FfiChangeReceiver`, uses a push model: the host calls `start_push_fn`
/// with a callback, and the plugin spawns a forwarder that pushes events via
/// the callback until the stream is exhausted (sends null) or the callback
/// returns `false`.
#[repr(C)]
pub struct FfiBootstrapReceiver {
    pub state: *mut c_void,
    pub start_push_fn: extern "C" fn(
        state: *mut c_void,
        callback: FfiBootstrapPushCallbackFn,
        callback_ctx: *mut c_void,
    ),
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

/// Callback for push-based query result delivery to reactions.
/// The plugin calls this to receive the next QueryResult from the host.
/// Returns a `*mut QueryResult` (ownership transfers to the plugin),
/// or null to signal end-of-stream / shutdown.
/// The `result` parameter is unused (reserved).
pub type FfiResultPushCallbackFn =
    extern "C" fn(ctx: *mut c_void, result: *mut c_void) -> *mut c_void;

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
    /// Nullable — snapshot fetcher for on-demand query snapshot access.
    pub snapshot_fetcher: *const SnapshotFetcherVtable,
}

// Safety: FfiRuntimeContext contains raw pointers that point to thread-safe data.
// The vtables contain only function pointers and Send+Sync state.
// The ctx pointers point to Arc-backed structures that are Send+Sync.
unsafe impl Send for FfiRuntimeContext {}
unsafe impl Sync for FfiRuntimeContext {}

// ============================================================================
// Source vtable
// ============================================================================

drasi_ffi_primitives::ffi_vtable! {
    /// FFI-safe vtable for a Source instance.
    pub struct SourceVtable {
        // Identity
        fn id_fn(state: *const) -> FfiStr,
        fn type_name_fn(state: *const) -> FfiStr,
        fn auto_start_fn(state: *const) -> bool,
        fn dispatch_mode_fn(state: *const) -> FfiDispatchMode,

        // Configuration inspection
        /// Returns the source's configuration properties as a JSON string.
        fn properties_fn(state: *const) -> FfiOwnedStr,

        // Lifecycle
        fn start_fn(state: *mut) -> FfiResult,
        fn stop_fn(state: *mut) -> FfiResult,
        fn status_fn(state: *const) -> FfiComponentStatus,
        fn deprovision_fn(state: *mut) -> FfiResult,

        // Initialization
        fn initialize_fn(state: *mut, ctx: *const FfiRuntimeContext),

        // Subscriptions
        /// Subscribe with query_id, node_labels JSON, relation_labels JSON,
        /// optional resume_from position bytes, and optional last_sequence
        /// for sequence counter recovery.
        fn subscribe_fn(state: *mut, source_id: FfiStr, enable_bootstrap: bool, query_id: FfiStr, nodes_json: FfiStr, relations_json: FfiStr, resume_from_ptr: *const u8, resume_from_len: u32, has_last_sequence: bool, last_sequence: u64) -> *mut FfiSubscriptionResponse,

        /// Host calls this to inject an external bootstrap provider (from another plugin).
        fn set_bootstrap_provider_fn(state: *mut, provider: *mut BootstrapProviderVtable),
    }
}

// ============================================================================
// Reaction vtable
// ============================================================================

// --- FFI types for the reaction bootstrap bridge ---

/// FFI-safe checkpoint data.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FfiCheckpoint {
    pub sequence: u64,
    pub config_hash: u64,
}

/// Result of a checkpoint read — `found=false` means no checkpoint exists.
#[repr(C)]
pub struct FfiCheckpointResult {
    pub found: bool,
    pub checkpoint: FfiCheckpoint,
    /// Non-null on error.
    pub error: FfiOwnedStr,
}

/// FFI-safe iterator for streaming snapshot rows one at a time.
///
/// The host creates this iterator and the plugin pulls rows via `next_fn`.
/// Each `next_fn` call returns one JSON-serialized row, or an empty string
/// when the iterator is exhausted.
///
/// **Ownership:** The plugin MUST call `drop_fn` when done — even if it
/// doesn't exhaust the iterator. The host allocates the iterator state
/// and `drop_fn` is the only way to reclaim it.
#[repr(C)]
pub struct FfiSnapshotIterator {
    /// Opaque host-allocated iterator state.
    pub iter_ctx: *mut c_void,
    /// Returns the next row as JSON, or an empty string when exhausted.
    pub next_fn: extern "C" fn(*mut c_void) -> FfiOwnedStr,
    /// Drops the iterator state. Must be called exactly once.
    pub drop_fn: extern "C" fn(*mut c_void),
}

/// FFI-safe response from a snapshot fetch — streaming variant.
///
/// On success (`error` is empty): `iterator` is valid and the plugin
/// pulls rows via `iterator.next_fn`. On error: `error` is non-empty
/// and `iterator` fields are invalid (must not be called).
#[repr(C)]
pub struct FfiSnapshotIteratorResponse {
    pub iterator: FfiSnapshotIterator,
    pub as_of_sequence: u64,
    pub config_hash: u64,
    /// Non-empty on error — iterator fields are invalid in that case.
    pub error: FfiOwnedStr,
}

/// FFI-safe iterator for streaming outbox entries one at a time.
///
/// Same ownership model as `FfiSnapshotIterator`.
#[repr(C)]
pub struct FfiOutboxIterator {
    /// Opaque host-allocated iterator state.
    pub iter_ctx: *mut c_void,
    /// Returns the next `QueryResult` as JSON, or an empty string when exhausted.
    pub next_fn: extern "C" fn(*mut c_void) -> FfiOwnedStr,
    /// Drops the iterator state. Must be called exactly once.
    pub drop_fn: extern "C" fn(*mut c_void),
}

/// FFI-safe response from an outbox fetch — streaming variant.
#[repr(C)]
pub struct FfiOutboxIteratorResponse {
    pub iterator: FfiOutboxIterator,
    pub latest_sequence: u64,
    pub config_hash: u64,
    /// Non-empty on error — iterator fields are invalid in that case.
    pub error: FfiOwnedStr,
}

/// Callback signatures for FfiBootstrapContext.
pub type FfiBootstrapFetchSnapshotFn = extern "C" fn(*mut c_void) -> FfiSnapshotIteratorResponse;
pub type FfiBootstrapFetchOutboxFn = extern "C" fn(*mut c_void, u64) -> FfiOutboxIteratorResponse;
pub type FfiBootstrapReadCheckpointFn = extern "C" fn(*mut c_void) -> FfiCheckpointResult;
pub type FfiBootstrapWriteCheckpointFn = extern "C" fn(*mut c_void, FfiCheckpoint) -> FfiResult;

/// FFI-safe bootstrap context passed to `bootstrap_fn`.
///
/// The host builds this and passes a pointer. The plugin calls the callback
/// function pointers with `callback_ctx` to invoke `fetch_snapshot()`, etc.
#[repr(C)]
pub struct FfiBootstrapContext {
    /// Query ID this bootstrap is for.
    pub query_id: FfiStr,
    /// `true` when a prior checkpoint is being discarded (reset/recovery).
    /// `false` on a fresh start with no prior checkpoint.
    pub is_reset: bool,
    /// Opaque host context passed as first arg to all callbacks.
    pub callback_ctx: *mut c_void,
    /// Fetch a snapshot of the query's live result set.
    pub fetch_snapshot_fn: FfiBootstrapFetchSnapshotFn,
    /// Fetch outbox entries after a given sequence.
    pub fetch_outbox_fn: FfiBootstrapFetchOutboxFn,
    /// Read the persisted checkpoint for this query subscription.
    pub read_checkpoint_fn: FfiBootstrapReadCheckpointFn,
    /// Write a checkpoint for this query subscription.
    pub write_checkpoint_fn: FfiBootstrapWriteCheckpointFn,
}

drasi_ffi_primitives::ffi_vtable! {
    /// FFI-safe vtable for a Reaction instance.
    pub struct ReactionVtable {
        // Identity
        fn id_fn(state: *const) -> FfiStr,
        fn type_name_fn(state: *const) -> FfiStr,
        fn auto_start_fn(state: *const) -> bool,
        fn query_ids_fn(state: *const) -> FfiStringArray,

        // Configuration inspection
        /// Returns the reaction's configuration properties as a JSON string.
        fn properties_fn(state: *const) -> FfiOwnedStr,

        // Lifecycle
        fn start_fn(state: *mut) -> FfiResult,
        fn stop_fn(state: *mut) -> FfiResult,
        fn status_fn(state: *const) -> FfiComponentStatus,
        fn deprovision_fn(state: *mut) -> FfiResult,

        // Initialization
        fn initialize_fn(state: *mut, ctx: *const FfiRuntimeContext),

        // Host-managed query subscription forwarding (push-based)
        /// The host calls this once to start push-based delivery.
        /// The plugin spawns a forwarder task that reads from an internal channel
        /// and calls `reaction.enqueue_query_result()` for each item.
        fn start_result_push_fn(state: *mut, callback: FfiResultPushCallbackFn, callback_ctx: *mut c_void),

        // Recovery archetype methods
        /// Whether this reaction requires a durable state store.
        fn is_durable_fn(state: *const) -> bool,
        /// Whether this reaction needs a full snapshot on first start.
        fn needs_snapshot_on_fresh_start_fn(state: *const) -> bool,
        /// Default recovery policy (returned as u8 ordinal: 0=Strict, 1=AutoReset, 2=AutoSkipGap).
        fn default_recovery_policy_fn(state: *const) -> u8,

        // Bootstrap hook
        /// Called during startup when bootstrap or recovery is needed.
        /// The FfiBootstrapContext provides callbacks for fetch_snapshot, fetch_outbox,
        /// and checkpoint read/write.
        fn bootstrap_fn(state: *mut, ctx: *const FfiBootstrapContext) -> FfiResult,
    }
}

// ============================================================================
// Bootstrap provider vtable
// ============================================================================

drasi_ffi_primitives::ffi_vtable! {
    /// FFI-safe vtable for a BootstrapProvider.
    /// The bootstrap plugin creates this; the host wraps it and passes it to the source plugin.
    pub struct BootstrapProviderVtable {
        /// Perform bootstrap. Sends records via the FfiBootstrapSender.
        /// Returns the count of records sent (>= 0), or negative on error.
        fn bootstrap_fn(state: *mut, query_id: FfiStr, node_labels: *const FfiStr, node_labels_count: usize, relation_labels: *const FfiStr, relation_labels_count: usize, request_id: FfiStr, server_id: FfiStr, source_id: FfiStr, sender: *mut FfiBootstrapSender) -> i64,
    }
}

// ============================================================================
// Plugin descriptor vtables — factories that create Source/Reaction instances
// ============================================================================

drasi_ffi_primitives::ffi_vtable! {
    /// FFI-safe vtable for a SourcePluginDescriptor (factory).
    /// The host calls `create_source_fn` to construct a SourceVtable from config.
    pub struct SourcePluginVtable {
        fn kind_fn(state: *const) -> FfiStr,
        fn config_version_fn(state: *const) -> FfiStr,
        fn config_schema_json_fn(state: *const) -> FfiOwnedStr,
        fn config_schema_name_fn(state: *const) -> FfiStr,

        fn create_source_fn(state: *mut, id: FfiStr, config_json: FfiStr, auto_start: bool) -> FfiCreateResult,
    }
}

drasi_ffi_primitives::ffi_vtable! {
    /// FFI-safe vtable for a ReactionPluginDescriptor (factory).
    pub struct ReactionPluginVtable {
        fn kind_fn(state: *const) -> FfiStr,
        fn config_version_fn(state: *const) -> FfiStr,
        fn config_schema_json_fn(state: *const) -> FfiOwnedStr,
        fn config_schema_name_fn(state: *const) -> FfiStr,

        /// Factory: create a ReactionVtable from JSON config.
        fn create_reaction_fn(state: *mut, id: FfiStr, query_ids_json: FfiStr, config_json: FfiStr, auto_start: bool) -> FfiCreateResult,
    }
}

drasi_ffi_primitives::ffi_vtable! {
    /// FFI-safe vtable for a BootstrapPluginDescriptor (factory).
    pub struct BootstrapPluginVtable {
        fn kind_fn(state: *const) -> FfiStr,
        fn config_version_fn(state: *const) -> FfiStr,
        fn config_schema_json_fn(state: *const) -> FfiOwnedStr,
        fn config_schema_name_fn(state: *const) -> FfiStr,

        /// Factory: create a BootstrapProviderVtable from JSON config.
        fn create_bootstrap_provider_fn(state: *mut, config_json: FfiStr, source_config_json: FfiStr) -> FfiCreateResult,
    }
}

drasi_ffi_primitives::ffi_vtable! {
    /// FFI-safe vtable for an IdentityProviderPluginDescriptor (factory).
    /// The host calls `create_identity_provider_fn` to construct an
    /// `IdentityProviderVtable` from config JSON.
    pub struct IdentityProviderPluginVtable {
        fn kind_fn(state: *const) -> FfiStr,
        fn config_version_fn(state: *const) -> FfiStr,
        fn config_schema_json_fn(state: *const) -> FfiOwnedStr,
        fn config_schema_name_fn(state: *const) -> FfiStr,

        /// Factory: create an IdentityProviderVtable from JSON config.
        fn create_identity_provider_fn(state: *mut, config_json: FfiStr) -> *mut super::identity::IdentityProviderVtable,
    }
}

// ============================================================================
// State store vtable — reverse direction (host → plugin)
// ============================================================================

/// State store vtable — host creates and provides to plugins.
/// Plugins call these function pointers to access persistent state.
#[repr(C)]
pub struct StateStoreVtable {
    pub state: *mut c_void,
    // Basic operations
    pub get_fn: extern "C" fn(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiGetResult,
    pub set_fn: extern "C" fn(
        state: *mut c_void,
        store_id: FfiStr,
        key: FfiStr,
        value: *const u8,
        value_len: usize,
    ) -> FfiResult,
    pub delete_fn: extern "C" fn(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiResult,
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
    pub list_keys_fn: extern "C" fn(state: *mut c_void, store_id: FfiStr) -> FfiStringArray,
    pub store_exists_fn: extern "C" fn(state: *mut c_void, store_id: FfiStr) -> FfiResult,
    pub key_count_fn: extern "C" fn(state: *mut c_void, store_id: FfiStr) -> i64,
    pub sync_fn: extern "C" fn(state: *mut c_void) -> FfiResult,
    // Cleanup
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for StateStoreVtable {}
unsafe impl Sync for StateStoreVtable {}

// ============================================================================
// Snapshot fetcher vtable — host creates and provides to plugins for on-demand
// query snapshot access at runtime (not just during bootstrap).
// ============================================================================

/// Snapshot fetcher vtable — host creates and provides to plugins.
///
/// Plugins call `fetch_snapshot_fn` to get the current result set of a query
/// as a streaming iterator. The callback accepts a `query_id` parameter
/// (unlike bootstrap callbacks which are per-query).
///
/// Follows the same ownership pattern as [`StateStoreVtable`]:
/// - Host `Box::into_raw`s an `Arc<dyn SnapshotFetcher>` into `state`
/// - Plugin calls `fetch_snapshot_fn` with `state` and `query_id`
/// - `drop_fn` reclaims the `Arc` when the plugin drops the proxy
#[repr(C)]
pub struct SnapshotFetcherVtable {
    /// Opaque host-owned state (Arc<dyn SnapshotFetcher> behind a raw pointer).
    pub state: *mut c_void,
    /// Fetch a snapshot for the given query ID.
    /// Returns an `FfiSnapshotIteratorResponse` with a streaming iterator on success,
    /// or a non-empty error string on failure.
    pub fetch_snapshot_fn:
        extern "C" fn(state: *mut c_void, query_id: FfiStr) -> FfiSnapshotIteratorResponse,
    /// Drop the host-owned state. Must be called exactly once when the plugin
    /// no longer needs the fetcher.
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

unsafe impl Send for SnapshotFetcherVtable {}
unsafe impl Sync for SnapshotFetcherVtable {}

// ============================================================================
// Plugin registration — returned by drasi_plugin_init() for cdylib builds
// ============================================================================

/// FFI-safe plugin registration returned by `drasi_plugin_init()`.
/// Contains factory vtables for all plugin types this shared library provides.
///
/// **ABI note**: This struct is `#[repr(C)]`. New fields must always be appended
/// at the end so that plugins compiled against an older SDK layout remain
/// compatible (the host simply treats trailing fields as absent).
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
    pub set_lifecycle_callback: extern "C" fn(
        ctx: *mut ::std::ffi::c_void,
        callback: super::callbacks::LifecycleCallbackFn,
    ),
    // --- Fields below were added after the initial ABI. They MUST remain at
    // the end so that older plugin binaries (which allocate a smaller struct)
    // are still layout-compatible with the host.
    pub identity_provider_plugins: *mut IdentityProviderPluginVtable,
    pub identity_provider_plugin_count: usize,
}
