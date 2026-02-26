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
use std::sync::{Arc, Mutex, OnceLock};

use drasi_lib::channels::events::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType,
};
use drasi_lib::managers::{ComponentEventHistory, ComponentLogRegistry, LogLevel, LogMessage};
use drasi_plugin_sdk::ffi::{
    FfiLifecycleEvent, FfiLifecycleEventType, FfiLogEntry, FfiLogLevel, LifecycleCallbackFn,
    LogCallbackFn,
};
use tokio::sync::RwLock;

/// Host-side callback context that routes plugin logs and events into DrasiLib.
///
/// One context is created per DrasiLib instance. The host passes a raw pointer
/// to this struct as the `ctx` argument when setting callbacks on plugins.
pub struct CallbackContext {
    /// The DrasiLib instance ID that owns the plugins using this context.
    pub instance_id: String,
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
/// Uses the `ComponentEventSender` channel from the SourceRuntimeContext so
/// lifecycle events flow through the same path as static sources (channel →
/// LifecycleManager → SourceManager.event_history).
pub struct InstanceCallbackContext {
    /// The DrasiLib instance ID.
    pub instance_id: String,
    /// The global log registry.
    pub log_registry: Arc<ComponentLogRegistry>,
    /// Channel for lifecycle events (same one the SourceManager monitors).
    pub event_tx: ComponentEventSender,
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

/// A captured log entry from a plugin (for testing/diagnostics).
#[derive(Debug, Clone)]
pub struct CapturedLog {
    pub level: FfiLogLevel,
    pub plugin_id: String,
    pub message: String,
}

/// A captured lifecycle event from a plugin (for testing/diagnostics).
#[derive(Debug, Clone)]
pub struct CapturedLifecycle {
    pub component_id: String,
    pub event_type: FfiLifecycleEventType,
    pub message: String,
}

/// Access the global captured log store (for testing/diagnostics).
pub fn captured_logs() -> &'static Mutex<Vec<CapturedLog>> {
    static LOGS: OnceLock<Mutex<Vec<CapturedLog>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Access the global captured lifecycle event store (for testing/diagnostics).
pub fn captured_lifecycles() -> &'static Mutex<Vec<CapturedLifecycle>> {
    static EVENTS: OnceLock<Mutex<Vec<CapturedLifecycle>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
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
pub extern "C" fn default_log_callback(ctx: *mut c_void, entry: *const FfiLogEntry) {
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

    // Always capture for diagnostics
    captured_logs().lock().unwrap().push(CapturedLog {
        level,
        plugin_id: plugin_id.clone(),
        message: message.clone(),
    });

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
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(registry.log(log_message));
        })
        .join();
    }
}

/// Host lifecycle callback that routes plugin events into DrasiLib's ComponentEventHistory.
pub extern "C" fn default_lifecycle_callback(ctx: *mut c_void, event: *const FfiLifecycleEvent) {
    let event = unsafe { &*event };
    let component_id = unsafe { event.component_id.to_string() };
    let component_type_str = unsafe { event.component_type.to_string() };
    let message = unsafe { event.message.to_string() };
    let event_type = event.event_type;

    log::debug!(
        "Lifecycle: {} ({}) {:?} {}",
        component_id,
        component_type_str,
        event_type,
        message
    );

    // Always capture for diagnostics
    captured_lifecycles().lock().unwrap().push(CapturedLifecycle {
        component_id: component_id.clone(),
        event_type,
        message: message.clone(),
    });

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

        // Route to the correct event history based on component type
        let event_history = match component_type {
            ComponentType::Reaction => context.reaction_event_history.clone(),
            _ => context.source_event_history.clone(),
        };

        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                event_history.write().await.record_event(component_event);
            });
        })
        .join();
    }
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
pub extern "C" fn instance_log_callback(ctx: *mut c_void, entry: *const FfiLogEntry) {
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
        if component_id.is_empty() { &plugin_id } else { &component_id },
        message
    );

    // Capture for diagnostics
    captured_logs().lock().unwrap().push(CapturedLog {
        level,
        plugin_id: plugin_id.clone(),
        message: message.clone(),
    });

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
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(registry.log(log_message));
        })
        .join();
    }
}

/// Per-instance lifecycle callback that sends events through the SourceManager's
/// event channel, so they flow through the same path as static source events.
pub extern "C" fn instance_lifecycle_callback(
    ctx: *mut c_void,
    event: *const FfiLifecycleEvent,
) {
    let event = unsafe { &*event };
    let component_id = unsafe { event.component_id.to_string() };
    let component_type_str = unsafe { event.component_type.to_string() };
    let message = unsafe { event.message.to_string() };
    let event_type = event.event_type;

    log::debug!(
        "Lifecycle [instance]: {} ({}) {:?} {}",
        component_id,
        component_type_str,
        event_type,
        message
    );

    // Capture for diagnostics
    captured_lifecycles().lock().unwrap().push(CapturedLifecycle {
        component_id: component_id.clone(),
        event_type,
        message: message.clone(),
    });

    // Send through the event channel (same path as static sources)
    if !ctx.is_null() {
        let context = unsafe { InstanceCallbackContext::from_raw_ref(ctx) };
        let component_type = parse_component_type(&component_type_str);
        let status = ffi_lifecycle_to_component_status(event_type);

        let component_event = ComponentEvent {
            component_id,
            component_type,
            status,
            timestamp: chrono::Utc::now(),
            message: if message.is_empty() {
                None
            } else {
                Some(message)
            },
        };

        let tx = context.event_tx.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Err(e) = tx.send(component_event).await {
                    log::error!("Failed to send lifecycle event: {}", e);
                }
            });
        })
        .join();
    }
}

/// Get the per-instance log callback function pointer.
pub fn instance_log_callback_fn() -> LogCallbackFn {
    instance_log_callback
}

/// Get the per-instance lifecycle callback function pointer.
pub fn instance_lifecycle_callback_fn() -> LifecycleCallbackFn {
    instance_lifecycle_callback
}
