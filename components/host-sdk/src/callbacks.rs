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

//! Log and lifecycle callback wiring for host-side plugin management.
//!
//! The host creates a [`CallbackContext`] per DrasiLib instance and passes it
//! as an opaque `*mut c_void` to each plugin. The callbacks then route logs
//! and lifecycle events into the DrasiLib systems that the REST API reads from.

use std::ffi::c_void;
use std::sync::Arc;

use drasi_lib::channels::events::{ComponentEvent, ComponentStatus, ComponentType};
use drasi_lib::component_graph::{ComponentUpdate, ComponentUpdateSender};
use drasi_lib::managers::{ComponentEventHistory, ComponentLogRegistry, LogLevel, LogMessage};
use drasi_plugin_sdk::ffi::{
    FfiLifecycleEvent, FfiLifecycleEventType, FfiLogEntry, FfiLogLevel, FfiLogLevelFilter,
    LifecycleCallbackFn, LogCallbackFn,
};
use tokio::sync::RwLock;

/// Spawn an async future on the host tokio runtime.
///
/// Callbacks are `extern "C"` functions invoked from within a plugin's own tokio
/// runtime, so we cannot call `block_on` on the host runtime. Instead we
/// Host-side callback context that routes plugin logs and events into DrasiLib.
///
/// One context is created per DrasiLib instance. The host passes a raw pointer
/// to this struct as the `ctx` argument when setting callbacks on plugins.
pub struct CallbackContext {
    /// The DrasiLib instance ID that owns the plugins using this context.
    pub instance_id: String,
    /// Handle to the host tokio runtime for dispatching async callback work.
    pub runtime_handle: tokio::runtime::Handle,
    /// The global log registry (shared across all DrasiLib instances).
    pub log_registry: Arc<ComponentLogRegistry>,
    /// The event history for the owning DrasiLib instance's sources.
    pub source_event_history: Arc<RwLock<ComponentEventHistory>>,
    /// The event history for the owning DrasiLib instance's reactions.
    pub reaction_event_history: Arc<RwLock<ComponentEventHistory>>,
}

// Safety: CallbackContext only contains Arc/RwLock types which are Send+Sync.
unsafe impl Send for CallbackContext {}
unsafe impl Sync for CallbackContext {}

impl CallbackContext {
    /// Convert to a raw pointer for passing through FFI.
    /// The caller must ensure the context lives as long as plugins use it.
    pub fn into_raw(self: Arc<Self>) -> *mut c_void {
        Arc::into_raw(self) as *mut c_void
    }

    /// Reconstruct from a raw pointer (does NOT take ownership — just borrows).
    ///
    /// # Safety
    /// The pointer must have been created by `into_raw` and the `Arc` must still be alive.
    unsafe fn from_raw_ref<'a>(ptr: *mut c_void) -> &'a Self {
        &*(ptr as *const Self)
    }
}

/// Per-source/reaction-instance callback context.
///
/// Created during `SourceProxy.initialize()` / `ReactionProxy.initialize()`.
/// Uses the `ComponentUpdateSender` channel from the runtime context so
/// lifecycle events flow through the ComponentGraph update loop.
pub struct InstanceCallbackContext {
    /// The DrasiLib instance ID.
    pub instance_id: String,
    /// Handle to the host tokio runtime for dispatching async callback work.
    pub runtime_handle: tokio::runtime::Handle,
    /// The global log registry.
    pub log_registry: Arc<ComponentLogRegistry>,
    /// Channel for status updates to the ComponentGraph.
    pub update_tx: ComponentUpdateSender,
}

// Safety: contains only Arc and tokio mpsc::Sender (which is Send+Sync).
unsafe impl Send for InstanceCallbackContext {}
unsafe impl Sync for InstanceCallbackContext {}

impl InstanceCallbackContext {
    pub fn into_raw(self: Arc<Self>) -> *mut c_void {
        Arc::into_raw(self) as *mut c_void
    }

    unsafe fn from_raw_ref<'a>(ptr: *mut c_void) -> &'a Self {
        &*(ptr as *const Self)
    }
}

/// Compute the host's effective log level for plugin log forwarding.
///
/// Passed to plugins at load time so they can drop filtered-out records
/// before formatting them or crossing the FFI. Sources, in order:
///
/// 1. The tracing subscriber's max level, when a subscriber is installed.
///    drasi-lib initializes tracing with an `EnvFilter`, so this reflects
///    `RUST_LOG` (the `log` crate's max level does not: the LogTracer bridge
///    pins it at `Trace`). `RUST_LOG=off` is honored: plugins are told `Off`
///    and forward nothing.
/// 2. The `log` crate's max level, when set to something other than `Off`
///    (env_logger-style hosts; see the NOTE below on why `Off` cannot be
///    honored from this source).
/// 3. `Trace` when neither is configured (e.g. tests): forward everything.
///
/// The tracing max level collapses per-target directives to the most
/// verbose one (`RUST_LOG=info,my_module=trace` reports `Trace`), so
/// plugins may over-forward relative to the host filter but never
/// under-forward.
pub fn effective_host_log_level() -> FfiLogLevelFilter {
    use tracing::level_filters::LevelFilter as TracingFilter;
    // `LevelFilter::current()` alone cannot distinguish "no subscriber
    // installed" from "subscriber installed with everything filtered off":
    // both read OFF. Ask the dispatcher whether a real subscriber is active
    // so a host configured for silence gets silence, not everything.
    let tracing_installed =
        tracing::dispatcher::get_default(|d| !d.is::<tracing::subscriber::NoSubscriber>());
    if tracing_installed {
        return match TracingFilter::current() {
            TracingFilter::OFF => FfiLogLevelFilter::Off,
            TracingFilter::ERROR => FfiLogLevelFilter::Error,
            TracingFilter::WARN => FfiLogLevelFilter::Warn,
            TracingFilter::INFO => FfiLogLevelFilter::Info,
            TracingFilter::DEBUG => FfiLogLevelFilter::Debug,
            _ => FfiLogLevelFilter::Trace,
        };
    }
    let log_level = log::max_level();
    if log_level != log::LevelFilter::Off {
        return FfiLogLevelFilter::from_level_filter(log_level);
    }
    // NOTE: the `log` crate reads `Off` both when no logger was ever
    // installed (the default) and when a pure-`log` host explicitly
    // configured silence (e.g. env_logger with RUST_LOG=off); its public
    // API cannot distinguish the two. We bias toward forwarding so
    // unconfigured hosts and bare test binaries still see plugin logs. A
    // `log`-only host that wants silence should call
    // `LoadedPlugin::set_log_level(FfiLogLevelFilter::Off)` after its
    // logging setup.
    FfiLogLevelFilter::Trace
}

fn ffi_log_level_to_log_level(level: FfiLogLevel) -> LogLevel {
    match level {
        FfiLogLevel::Error => LogLevel::Error,
        FfiLogLevel::Warn => LogLevel::Warn,
        FfiLogLevel::Info => LogLevel::Info,
        FfiLogLevel::Debug => LogLevel::Debug,
        FfiLogLevel::Trace => LogLevel::Trace,
    }
}

fn ffi_log_level_to_std_level(level: FfiLogLevel) -> log::Level {
    match level {
        FfiLogLevel::Error => log::Level::Error,
        FfiLogLevel::Warn => log::Level::Warn,
        FfiLogLevel::Info => log::Level::Info,
        FfiLogLevel::Debug => log::Level::Debug,
        FfiLogLevel::Trace => log::Level::Trace,
    }
}

fn parse_component_type(s: &str) -> ComponentType {
    match s {
        "source" => ComponentType::Source,
        "query" => ComponentType::Query,
        "reaction" => ComponentType::Reaction,
        _ => ComponentType::Source, // default for "plugin" or unknown
    }
}

fn ffi_lifecycle_to_component_status(event_type: FfiLifecycleEventType) -> ComponentStatus {
    match event_type {
        FfiLifecycleEventType::Starting => ComponentStatus::Starting,
        FfiLifecycleEventType::Started => ComponentStatus::Running,
        FfiLifecycleEventType::Stopping => ComponentStatus::Stopping,
        FfiLifecycleEventType::Stopped => ComponentStatus::Stopped,
        FfiLifecycleEventType::Error => ComponentStatus::Error,
    }
}

/// Host log callback that routes plugin logs into the DrasiLib ComponentLogRegistry.
///
/// When `ctx` is non-null and points to a valid [`CallbackContext`], AND the
/// FfiLogEntry carries a non-empty `instance_id` and `component_id`, logs are
/// pushed into the registry with the correct composite key so they appear in
/// the REST API's log streaming endpoints.
/// # Safety
/// `entry` must be a valid pointer to an `FfiLogEntry`. `ctx` may be null (logs
/// are still forwarded to the host log framework), or must point to a valid
/// `CallbackContext` for registry routing.
///
/// This function's signature matches `LogCallbackFn` (non-unsafe `extern "C"`).
/// Raw pointer dereferences are guarded by `unsafe` blocks inside the body.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn default_log_callback(ctx: *mut c_void, entry: *const FfiLogEntry) {
    // Wrap the entire body in catch_unwind: this is an extern "C" function called
    // from plugin code, so any unwinding panic across the FFI boundary causes a
    // non-unwinding abort. We must absorb panics here.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let entry = unsafe { &*entry };
        let plugin_id = unsafe { entry.plugin_id.to_string() };
        let message = unsafe { entry.message.to_string() };
        let instance_id = unsafe { entry.instance_id.to_string() };
        let component_id = unsafe { entry.component_id.to_string() };
        let level = entry.level;

        // Always forward to host's log framework
        log::log!(
            ffi_log_level_to_std_level(level),
            "[plugin:{}] {}",
            if component_id.is_empty() {
                &plugin_id
            } else {
                &component_id
            },
            message
        );

        // Route into DrasiLib's ComponentLogRegistry if we have both context and instance info
        if !ctx.is_null() && !instance_id.is_empty() && !component_id.is_empty() {
            let context = unsafe { CallbackContext::from_raw_ref(ctx) };
            let log_message = LogMessage::with_instance(
                ffi_log_level_to_log_level(level),
                message,
                &instance_id,
                &component_id,
                ComponentType::Source, // TODO: parse from entry if available
            );
            let registry = context.log_registry.clone();
            // try_log may panic from inside tokio's RwLock::try_write under
            // certain race conditions; catch_unwind above absorbs that.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.try_log(log_message);
            }));
        }
    }));
}

/// Host lifecycle callback that routes plugin events into DrasiLib's ComponentEventHistory.
/// # Safety
/// `event` must be a valid pointer to an `FfiLifecycleEvent`. `ctx` may be null
/// or must point to a valid `CallbackContext`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn default_lifecycle_callback(ctx: *mut c_void, event: *const FfiLifecycleEvent) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let event = unsafe { &*event };
        let component_id = unsafe { event.component_id.to_string() };
        let component_type_str = unsafe { event.component_type.to_string() };
        let message = unsafe { event.message.to_string() };
        let event_type = event.event_type;

        log::debug!("Lifecycle: {component_id} ({component_type_str}) {event_type:?} {message}");

        // Route into DrasiLib's ComponentEventHistory if context is available
        if !ctx.is_null() {
            let context = unsafe { CallbackContext::from_raw_ref(ctx) };
            let component_type = parse_component_type(&component_type_str);
            let status = ffi_lifecycle_to_component_status(event_type);

            let component_event = ComponentEvent {
                component_id,
                component_type: component_type.clone(),
                status,
                timestamp: chrono::Utc::now(),
                message: if message.is_empty() {
                    None
                } else {
                    Some(message)
                },
            };

            // Use try_write to avoid spawning async tasks that block the scheduler
            let event_history = match component_type {
                ComponentType::Reaction => context.reaction_event_history.clone(),
                _ => context.source_event_history.clone(),
            };
            // try_write on tokio RwLock may panic with `unreachable!` under certain
            // race conditions; absorb that panic to keep FFI safe.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(mut history) = event_history.try_write() {
                    history.record_event(component_event);
                }
            }));
        }
    }));
}

/// Get the default log callback function pointer.
pub fn default_log_callback_fn() -> LogCallbackFn {
    default_log_callback
}

/// Get the default lifecycle callback function pointer.
pub fn default_lifecycle_callback_fn() -> LifecycleCallbackFn {
    default_lifecycle_callback
}

// ============================================================================
// Per-instance callbacks (used by SourceProxy/ReactionProxy)
// ============================================================================

/// Per-instance log callback that routes logs using InstanceCallbackContext.
///
/// This callback is set during SourceProxy.initialize() via FfiRuntimeContext.
/// It uses the `instance_id` and `component_id` from the FfiLogEntry (set by
/// the plugin's TLS-aware FfiLogger) to construct the correct ComponentLogKey.
/// # Safety
/// `entry` must be a valid pointer to an `FfiLogEntry`. `ctx` may be null or
/// must point to a valid `InstanceCallbackContext`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn instance_log_callback(ctx: *mut c_void, entry: *const FfiLogEntry) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let entry = unsafe { &*entry };
        let plugin_id = unsafe { entry.plugin_id.to_string() };
        let message = unsafe { entry.message.to_string() };
        let instance_id = unsafe { entry.instance_id.to_string() };
        let component_id = unsafe { entry.component_id.to_string() };
        let level = entry.level;

        // Forward to host log framework
        log::log!(
            ffi_log_level_to_std_level(level),
            "[plugin:{}] {}",
            if component_id.is_empty() {
                &plugin_id
            } else {
                &component_id
            },
            message
        );

        // Route into ComponentLogRegistry
        if !ctx.is_null() {
            let context = unsafe { InstanceCallbackContext::from_raw_ref(ctx) };
            // Use instance_id/component_id from the log entry (set by TLS in plugin)
            // Fall back to context's instance_id if entry doesn't have them
            let log_instance_id = if instance_id.is_empty() {
                &context.instance_id
            } else {
                &instance_id
            };
            let log_component_id = if component_id.is_empty() {
                &plugin_id
            } else {
                &component_id
            };
            let log_message = LogMessage::with_instance(
                ffi_log_level_to_log_level(level),
                message,
                log_instance_id,
                log_component_id,
                ComponentType::Source,
            );
            let registry = context.log_registry.clone();
            // Use try_log (non-blocking) to avoid spawning async tasks that can
            // block the current_thread scheduler during drop sequences.
            // try_log internally calls tokio's RwLock::try_write which can panic
            // with `unreachable!` under certain races; absorb that panic.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.try_log(log_message);
            }));
        }
    }));
}

/// Per-instance lifecycle callback that sends events through the SourceManager's
/// event channel, so they flow through the same path as static source events.
/// # Safety
/// `event` must be a valid pointer to an `FfiLifecycleEvent`. `ctx` may be null
/// or must point to a valid `InstanceCallbackContext`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn instance_lifecycle_callback(ctx: *mut c_void, event: *const FfiLifecycleEvent) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let event = unsafe { &*event };
        let component_id = unsafe { event.component_id.to_string() };
        let component_type_str = unsafe { event.component_type.to_string() };
        let message = unsafe { event.message.to_string() };
        let event_type = event.event_type;

        log::debug!(
            "Lifecycle [instance]: {component_id} ({component_type_str}) {event_type:?} {message}"
        );

        // Send through the component graph update channel
        if !ctx.is_null() {
            let context = unsafe { InstanceCallbackContext::from_raw_ref(ctx) };
            let status = ffi_lifecycle_to_component_status(event_type);

            let update = ComponentUpdate::Status {
                component_id,
                status,
                message: if message.is_empty() {
                    None
                } else {
                    Some(message)
                },
            };

            let tx = context.update_tx.clone();
            // Use try_send to avoid spawning an async task that may block
            // the host runtime's current_thread scheduler during drop sequences.
            if let Err(e) = tx.try_send(update) {
                log::error!("Failed to send lifecycle event: {e}");
            }
        }
    }));
}

/// Get the per-instance log callback function pointer.
pub fn instance_log_callback_fn() -> LogCallbackFn {
    instance_log_callback
}

/// Get the per-instance lifecycle callback function pointer.
pub fn instance_lifecycle_callback_fn() -> LifecycleCallbackFn {
    instance_lifecycle_callback
}
