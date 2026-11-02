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
use drasi_lib::channels::{ComponentUpdate, ComponentUpdateSender};
use drasi_lib::observability::{
    ComponentEventHistory, ComponentLogRegistry, ComponentLogType, LogLevel, LogMessage,
};
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
    /// The selected host-local log destination.
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
/// Uses the runtime's `ComponentUpdateSender` so lifecycle observations reach the
/// graph-owned adapter, including catch-up after a burst of callbacks.
pub struct InstanceCallbackContext {
    /// The DrasiLib instance ID.
    pub instance_id: String,
    /// Host-assigned identity and log category; plugin fields cannot reroute the record.
    pub component_id: String,
    pub component_type: ComponentLogType,
    /// Handle to the host tokio runtime for dispatching async callback work.
    pub runtime_handle: tokio::runtime::Handle,
    /// The selected host-local log destination.
    pub log_registry: Arc<ComponentLogRegistry>,
    /// Lifecycle mailbox supplied by the graph-owned adapter.
    pub update_tx: ComponentUpdateSender,
}

impl InstanceCallbackContext {
    pub fn into_raw(self: Arc<Self>) -> *mut c_void {
        Arc::into_raw(self) as *mut c_void
    }

    unsafe fn from_raw_ref<'a>(ptr: *mut c_void) -> &'a Self {
        &*(ptr as *const Self)
    }
}

/// Conservative log-only forwarding limit. Never infer this from tracing's
/// subscriber-wide maximum: operation-span layers can raise that independently.
/// Custom hosts can supply a policy to `load_plugin_from_path_with_log_policy`.
pub fn effective_host_log_level() -> FfiLogLevelFilter {
    match configured_host_log_level() {
        Ok(level) => FfiLogLevelFilter::from_level_filter(level.unwrap_or(log::LevelFilter::Trace)),
        Err(error) => {
            eprintln!("drasi-host-sdk: cannot determine logging policy: {error:#}");
            FfiLogLevelFilter::Off
        }
    }
}

pub(crate) fn configured_host_log_level() -> anyhow::Result<Option<log::LevelFilter>> {
    if let Some(level) = drasi_lib::observability::default_log_level()? {
        return Ok(Some(level));
    }
    let log_level = log::max_level();
    if log_level != log::LevelFilter::Off {
        return Ok(Some(log_level));
    }
    // The log facade cannot distinguish an explicit Off from no logger.
    Ok(None)
}

pub(crate) struct PluginLogForwarding {
    setter: Option<extern "C" fn(FfiLogLevelFilter)>,
    maximum: std::sync::Mutex<Option<log::LevelFilter>>,
}

impl PluginLogForwarding {
    pub(crate) fn shared(
        setter: Option<extern "C" fn(FfiLogLevelFilter)>,
        initial: Option<log::LevelFilter>,
    ) -> anyhow::Result<Arc<Self>> {
        type Forwarders = std::collections::HashMap<usize, std::sync::Weak<PluginLogForwarding>>;
        static FORWARDERS: std::sync::OnceLock<std::sync::Mutex<Forwarders>> =
            std::sync::OnceLock::new();
        let Some(setter) = setter else {
            return Ok(Arc::new(Self::new(None, initial)));
        };
        // Aliased/repeated dlopen calls share plugin globals. Libraries are
        // process-pinned, so the setter address identifies that shared state.
        let key = setter as usize;
        let mut forwarders = FORWARDERS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("shared plugin log forwarding state poisoned"))?;
        forwarders.retain(|_, state| state.strong_count() > 0);
        if let Some(existing) = forwarders.get(&key).and_then(std::sync::Weak::upgrade) {
            if let Some(level) = initial {
                existing.include(level)?;
            }
            return Ok(existing);
        }
        let forwarding = Arc::new(Self::new(Some(setter), initial));
        forwarders.insert(key, Arc::downgrade(&forwarding));
        Ok(forwarding)
    }

    pub(crate) fn new(
        setter: Option<extern "C" fn(FfiLogLevelFilter)>,
        initial: Option<log::LevelFilter>,
    ) -> Self {
        let forwarding = Self {
            setter,
            maximum: std::sync::Mutex::new(initial),
        };
        forwarding.apply(initial.unwrap_or(log::LevelFilter::Trace));
        forwarding
    }

    fn apply(&self, level: log::LevelFilter) {
        if let Some(setter) = self.setter {
            setter(FfiLogLevelFilter::from_level_filter(level));
        }
    }

    /// Retain the most verbose bound scope, without an unbounded scope table.
    /// Per-destination filtering still applies after a scope is retired.
    pub(crate) fn include(&self, level: log::LevelFilter) -> anyhow::Result<()> {
        let mut maximum = self
            .maximum
            .lock()
            .map_err(|_| anyhow::anyhow!("plugin log forwarding state poisoned"))?;
        let next = maximum.map_or(level, |current| current.max(level));
        *maximum = Some(next);
        self.apply(next);
        Ok(())
    }

    pub(crate) fn set(&self, level: FfiLogLevelFilter) -> anyhow::Result<()> {
        let level = match level {
            FfiLogLevelFilter::Off => log::LevelFilter::Off,
            FfiLogLevelFilter::Error => log::LevelFilter::Error,
            FfiLogLevelFilter::Warn => log::LevelFilter::Warn,
            FfiLogLevelFilter::Info => log::LevelFilter::Info,
            FfiLogLevelFilter::Debug => log::LevelFilter::Debug,
            FfiLogLevelFilter::Trace => log::LevelFilter::Trace,
        };
        let mut maximum = self
            .maximum
            .lock()
            .map_err(|_| anyhow::anyhow!("plugin log forwarding state poisoned"))?;
        *maximum = Some(level);
        self.apply(level);
        Ok(())
    }
}

pub(crate) fn log_binding_failure(
    updates: &ComponentUpdateSender,
    component_id: &str,
    error: anyhow::Error,
) {
    let message = format!("component log binding failed: {error:#}");
    eprintln!("drasi-host-sdk: {message}");
    if let Err(error) = updates.try_send(ComponentUpdate::Status {
        component_id: component_id.into(),
        status: ComponentStatus::Error,
        message: Some(message),
    }) {
        eprintln!("drasi-host-sdk: failed to report log binding failure: {error}");
    }
}

pub(crate) fn bind_log_policy(
    registry: &ComponentLogRegistry,
    forwarding: Option<&PluginLogForwarding>,
) -> anyhow::Result<()> {
    if let (Some(level), Some(forwarding)) = (registry.legacy_log_level()?, forwarding) {
        forwarding.include(level)?;
    }
    Ok(())
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
        let target = unsafe { entry.target.to_string() };
        let instance_id = unsafe { entry.instance_id.to_string() };
        let component_id = unsafe { entry.component_id.to_string() };
        let level = entry.level;

        // Always forward to host's log framework
        log::log!(
            target: drasi_lib::observability::PLUGIN_LOG_TARGET,
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
                report_log_admission(registry.try_log_with_target(&target, log_message));
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
/// Host-assigned instance/component identity and category determine the log key.
/// # Safety
/// `entry` must be a valid pointer to an `FfiLogEntry`. `ctx` may be null or
/// must point to a valid `InstanceCallbackContext`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn instance_log_callback(ctx: *mut c_void, entry: *const FfiLogEntry) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let entry = unsafe { &*entry };
        let plugin_id = unsafe { entry.plugin_id.to_string() };
        let message = unsafe { entry.message.to_string() };
        let target = unsafe { entry.target.to_string() };
        let component_id = unsafe { entry.component_id.to_string() };
        let level = entry.level;

        // Forward to host log framework
        log::log!(
            target: drasi_lib::observability::PLUGIN_LOG_TARGET,
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
            let log_message = LogMessage::with_instance(
                ffi_log_level_to_log_level(level),
                message,
                &context.instance_id,
                &context.component_id,
                context.component_type.clone(),
            );
            let registry = context.log_registry.clone();
            // Use try_log (non-blocking) to avoid spawning async tasks that can
            // block the current_thread scheduler during drop sequences.
            // try_log internally calls tokio's RwLock::try_write which can panic
            // with `unreachable!` under certain races; absorb that panic.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                report_log_admission(registry.try_log_with_target(&target, log_message));
            }));
        }
    }));
}

fn report_log_admission(result: drasi_lib::observability::LoggingResult<bool>) {
    if !matches!(result, Ok(true)) {
        static LOSSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = LOSSES
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        if count.is_power_of_two() {
            eprintln!(
                "drasi-host-sdk: component log destination rejected {count} records: {result:?}"
            );
        }
    }
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
            // The graph mailbox coalesces status bursts without losing failure
            // or readiness. No blocking callback or detached forwarding task.
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

#[cfg(test)]
mod forwarding_tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    static LEVEL: AtomicU8 = AtomicU8::new(0);
    extern "C" fn set_level(level: FfiLogLevelFilter) {
        LEVEL.store(level as u8, Ordering::SeqCst);
    }

    #[test]
    fn scopes_and_repeated_library_loads_share_a_conservative_ceiling() {
        let first = PluginLogForwarding::shared(Some(set_level), None).unwrap();
        first.include(log::LevelFilter::Info).unwrap();
        assert_eq!(LEVEL.load(Ordering::SeqCst), FfiLogLevelFilter::Info as u8);
        let second =
            PluginLogForwarding::shared(Some(set_level), Some(log::LevelFilter::Off)).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        second.include(log::LevelFilter::Off).unwrap();
        assert_eq!(LEVEL.load(Ordering::SeqCst), FfiLogLevelFilter::Info as u8);
        second.include(log::LevelFilter::Debug).unwrap();
        first.include(log::LevelFilter::Info).unwrap();
        assert_eq!(LEVEL.load(Ordering::SeqCst), FfiLogLevelFilter::Debug as u8);
        first.set(FfiLogLevelFilter::Off).unwrap();
        assert_eq!(LEVEL.load(Ordering::SeqCst), FfiLogLevelFilter::Off as u8);
    }
}
