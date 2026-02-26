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

//! Vtable generation functions.
//!
//! These functions wrap real DrasiLib `Source`, `Reaction`, and `BootstrapProvider`
//! trait implementations into FFI-safe vtables. Each `extern "C"` function pointer
//! handles:
//! - `catch_unwind` for panic safety at the FFI boundary
//! - Async→sync bridging via `std::thread::spawn` + `block_on`
//! - Lifecycle event emission
//! - Error → `FfiResult` conversion

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;

use drasi_core::models::{ElementMetadata, SourceChange};
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::channels::events::{
    BootstrapEvent, BootstrapEventSender, ComponentEventReceiver, SourceEvent, SourceEventWrapper,
};
use drasi_lib::channels::ChangeReceiver;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::reactions::Reaction;
use drasi_lib::sources::Source;
use drasi_lib::{ComponentStatus, DispatchMode, SourceRuntimeContext, StateStoreProvider};

use super::bootstrap_proxy::FfiBootstrapProviderProxy;
use super::callbacks::FfiLifecycleEvent;
use super::callbacks::FfiLifecycleEventType;
use super::state_store_proxy::FfiStateStoreProxy;
use super::types::*;
use super::vtables::*;
use crate::descriptor::{
    BootstrapPluginDescriptor, ReactionPluginDescriptor, SourcePluginDescriptor,
};

type LifecycleEmitterFn = fn(&str, FfiLifecycleEventType, &str);

/// Helper to extract ElementMetadata from a SourceChange.
fn source_change_metadata(change: &SourceChange) -> Option<&ElementMetadata> {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            Some(element.get_metadata())
        }
        SourceChange::Delete { metadata } => Some(metadata),
        SourceChange::Future { .. } => None,
    }
}

// ============================================================================
// Component status conversion
// ============================================================================

fn component_status_to_ffi(s: ComponentStatus) -> FfiComponentStatus {
    match s {
        ComponentStatus::Running => FfiComponentStatus::Running,
        ComponentStatus::Stopped => FfiComponentStatus::Stopped,
        ComponentStatus::Starting => FfiComponentStatus::Starting,
        ComponentStatus::Stopping => FfiComponentStatus::Stopping,
        ComponentStatus::Reconfiguring => FfiComponentStatus::Reconfiguring,
        ComponentStatus::Error => FfiComponentStatus::Error,
    }
}

fn dispatch_mode_to_ffi(m: DispatchMode) -> FfiDispatchMode {
    match m {
        DispatchMode::Broadcast => FfiDispatchMode::Broadcast,
        DispatchMode::Channel => FfiDispatchMode::Channel,
    }
}

// ============================================================================
// Thread-local context for per-instance log routing
// ============================================================================

/// Per-instance context set on TLS before calling into Source/Reaction methods.
/// The FfiLogger reads this to enrich log entries with the correct IDs.
#[derive(Clone)]
pub struct InstanceLogContext {
    pub instance_id: String,
    pub component_id: String,
    pub component_type: String,
    pub log_cb: Option<super::callbacks::LogCallbackFn>,
    pub log_ctx: *mut c_void,
}

// Safety: the raw pointer is only used for passing back to the host callback
unsafe impl Send for InstanceLogContext {}

thread_local! {
    /// Thread-local instance log context, set by vtable wrapper functions.
    pub static INSTANCE_LOG_CTX: std::cell::RefCell<Option<InstanceLogContext>> =
        const { std::cell::RefCell::new(None) };
}

/// Get the current per-instance log context (if any).
pub fn current_instance_log_ctx() -> Option<InstanceLogContext> {
    INSTANCE_LOG_CTX.with(|ctx| ctx.borrow().clone())
}

/// Set the TLS log context and run a closure.
fn with_instance_log_ctx<F, R>(ctx: Option<InstanceLogContext>, f: F) -> R
where
    F: FnOnce() -> R,
{
    INSTANCE_LOG_CTX.with(|tls| {
        *tls.borrow_mut() = ctx;
    });
    let result = f();
    INSTANCE_LOG_CTX.with(|tls| {
        *tls.borrow_mut() = None;
    });
    result
}

// ============================================================================
// Source vtable generation (concrete type)
// ============================================================================

/// Internal wrapper holding a Source impl + runtime handle + lifecycle emitter.
pub(crate) struct SourceWrapper<T: Source + 'static> {
    pub inner: T,
    pub cached_id: String,
    pub cached_type_name: String,
    pub lifecycle_emitter: LifecycleEmitterFn,
    pub runtime_handle: fn() -> &'static tokio::runtime::Runtime,
    pub vtable_executor: AsyncExecutorFn,
    /// Per-instance log callback (set during initialize, nullable).
    pub instance_log_cb: std::sync::atomic::AtomicPtr<()>,
    pub instance_log_ctx: std::sync::atomic::AtomicPtr<c_void>,
    /// Per-instance lifecycle callback (set during initialize, nullable).
    pub instance_lifecycle_cb: std::sync::atomic::AtomicPtr<()>,
    pub instance_lifecycle_ctx: std::sync::atomic::AtomicPtr<c_void>,
    /// Cached instance_id from FfiRuntimeContext (set during initialize).
    pub instance_id: std::sync::RwLock<String>,
    /// Keeps the dummy status_rx alive so inner source's status_tx.send() doesn't error.
    pub _status_rx: std::sync::Mutex<Option<ComponentEventReceiver>>,
}

/// Build a SourceVtable from a concrete type implementing the DrasiLib Source trait.
pub fn build_source_vtable<T: Source + 'static>(
    source: T,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime: fn() -> &'static tokio::runtime::Runtime,
) -> SourceVtable {
    extern "C" fn id_fn<T: Source + 'static>(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        FfiStr::from_str(&w.cached_id)
    }

    extern "C" fn type_name_fn<T: Source + 'static>(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        FfiStr::from_str(&w.cached_type_name)
    }

    extern "C" fn auto_start_fn<T: Source + 'static>(state: *const c_void) -> bool {
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        w.inner.auto_start()
    }

    extern "C" fn dispatch_mode_fn<T: Source + 'static>(state: *const c_void) -> FfiDispatchMode {
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        dispatch_mode_to_ffi(w.inner.dispatch_mode())
    }

    /// Emit a lifecycle event, preferring per-instance callback over global.
    fn emit_lifecycle_for<T: Source + 'static>(
        w: &SourceWrapper<T>,
        event_type: FfiLifecycleEventType,
        message: &str,
    ) {
        let cb_ptr = w
            .instance_lifecycle_cb
            .load(std::sync::atomic::Ordering::Acquire);
        if !cb_ptr.is_null() {
            let cb: super::callbacks::LifecycleCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
            let ctx = w
                .instance_lifecycle_ctx
                .load(std::sync::atomic::Ordering::Acquire);
            let instance_id = w.instance_id.read().map(|s| s.clone()).unwrap_or_default();
            let event = FfiLifecycleEvent {
                component_id: FfiStr::from_str(&w.cached_id),
                component_type: FfiStr::from_str("source"),
                event_type,
                message: FfiStr::from_str(message),
                timestamp_us: super::now_us(),
            };
            cb(ctx, &event);
        } else {
            // Fall back to global lifecycle emitter
            (w.lifecycle_emitter)(&w.cached_id, event_type, message);
        }
    }

    /// Build an InstanceLogContext from the wrapper's per-instance state.
    fn build_instance_log_ctx<T: Source + 'static>(
        w: &SourceWrapper<T>,
    ) -> Option<InstanceLogContext> {
        let cb_ptr = w.instance_log_cb.load(std::sync::atomic::Ordering::Acquire);
        if cb_ptr.is_null() {
            return None;
        }
        let cb: super::callbacks::LogCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
        let ctx = w
            .instance_log_ctx
            .load(std::sync::atomic::Ordering::Acquire);
        let instance_id = w.instance_id.read().map(|s| s.clone()).unwrap_or_default();
        Some(InstanceLogContext {
            instance_id,
            component_id: w.cached_id.clone(),
            component_type: "source".to_string(),
            log_cb: Some(cb),
            log_ctx: ctx,
        })
    }

    extern "C" fn start_fn<T: Source + 'static>(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const SourceWrapper<T>) };
            emit_lifecycle_for(w, FfiLifecycleEventType::Starting, "");
            let log_ctx = build_instance_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner = unsafe { &*(state as *const SourceWrapper<T>) };
                move || with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.start()))
            })
            .join()
            .expect("source start thread panicked");
            match result {
                Ok(()) => {
                    emit_lifecycle_for(w, FfiLifecycleEventType::Started, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_lifecycle_for(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn stop_fn<T: Source + 'static>(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const SourceWrapper<T>) };
            emit_lifecycle_for(w, FfiLifecycleEventType::Stopping, "");
            let log_ctx = build_instance_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner = unsafe { &*(state as *const SourceWrapper<T>) };
                move || with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.stop()))
            })
            .join()
            .expect("source stop thread panicked");
            match result {
                Ok(()) => {
                    emit_lifecycle_for(w, FfiLifecycleEventType::Stopped, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_lifecycle_for(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn status_fn<T: Source + 'static>(state: *const c_void) -> FfiComponentStatus {
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        let handle = (w.runtime_handle)().handle().clone();
        let status = std::thread::spawn({
            let inner = unsafe { &*(state as *const SourceWrapper<T>) };
            move || handle.block_on(inner.inner.status())
        })
        .join()
        .expect("source status thread panicked");
        component_status_to_ffi(status)
    }

    extern "C" fn deprovision_fn<T: Source + 'static>(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const SourceWrapper<T>) };
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner = unsafe { &*(state as *const SourceWrapper<T>) };
                move || handle.block_on(inner.inner.deprovision())
            })
            .join()
            .expect("source deprovision thread panicked");
            match result {
                Ok(()) => FfiResult::ok(),
                Err(e) => FfiResult::err(e.to_string()),
            }
        })
    }

    extern "C" fn subscribe_fn<T: Source + 'static>(
        state: *mut c_void,
        source_id: FfiStr,
        enable_bootstrap: bool,
        query_id: FfiStr,
        nodes_json: FfiStr,
        relations_json: FfiStr,
    ) -> *mut FfiSubscriptionResponse {
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        let source_id_str = unsafe { source_id.to_string() };
        let qid = unsafe { query_id.to_string() };
        let nodes_str = unsafe { nodes_json.to_string() };
        let rels_str = unsafe { relations_json.to_string() };

        let nodes: HashSet<String> = serde_json::from_str(&nodes_str).unwrap_or_default();
        let relations: HashSet<String> = serde_json::from_str(&rels_str).unwrap_or_default();

        let settings = SourceSubscriptionSettings {
            source_id: source_id_str,
            enable_bootstrap,
            query_id: qid,
            nodes,
            relations,
        };

        let handle = (w.runtime_handle)().handle().clone();
        let result = std::thread::spawn({
            let inner = unsafe { &*(state as *const SourceWrapper<T>) };
            move || handle.block_on(inner.inner.subscribe(settings))
        })
        .join()
        .expect("source subscribe thread panicked");

        match result {
            Ok(sub) => wrap_subscription_response(sub, w.vtable_executor),
            Err(e) => {
                log::error!("Subscribe failed: {}", e);
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn initialize_fn<T: Source + 'static>(
        state: *mut c_void,
        ctx: *const FfiRuntimeContext,
    ) {
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        let ffi_ctx = unsafe { &*ctx };

        // Capture per-instance callbacks from the runtime context
        if let Some(log_cb) = ffi_ctx.log_callback {
            w.instance_log_cb
                .store(log_cb as *mut (), std::sync::atomic::Ordering::Release);
            w.instance_log_ctx
                .store(ffi_ctx.log_ctx, std::sync::atomic::Ordering::Release);
        }
        if let Some(lifecycle_cb) = ffi_ctx.lifecycle_callback {
            w.instance_lifecycle_cb.store(
                lifecycle_cb as *mut (),
                std::sync::atomic::Ordering::Release,
            );
            w.instance_lifecycle_ctx
                .store(ffi_ctx.lifecycle_ctx, std::sync::atomic::Ordering::Release);
        }

        // Store instance_id for log context
        if let Ok(mut iid) = w.instance_id.write() {
            *iid = unsafe { ffi_ctx.instance_id.to_string() };
        }

        let (runtime_ctx, status_rx) = build_source_runtime_context(ffi_ctx);
        // Keep the receiver alive so inner source's status_tx.send() doesn't error
        if let Ok(mut guard) = w._status_rx.lock() {
            *guard = Some(status_rx);
        }
        let handle = (w.runtime_handle)().handle().clone();
        std::thread::spawn({
            let inner = unsafe { &*(state as *const SourceWrapper<T>) };
            move || handle.block_on(inner.inner.initialize(runtime_ctx))
        })
        .join()
        .expect("source initialize thread panicked");
    }

    extern "C" fn set_bootstrap_provider_fn<T: Source + 'static>(
        state: *mut c_void,
        provider: *mut BootstrapProviderVtable,
    ) {
        if provider.is_null() {
            return;
        }
        let vtable = unsafe { Box::from_raw(provider) };
        let proxy = Box::new(FfiBootstrapProviderProxy {
            vtable: std::sync::Mutex::new(*vtable),
        });
        let w = unsafe { &*(state as *const SourceWrapper<T>) };
        let handle = (w.runtime_handle)().handle().clone();
        std::thread::spawn({
            let inner = unsafe { &*(state as *const SourceWrapper<T>) };
            move || handle.block_on(inner.inner.set_bootstrap_provider(proxy))
        })
        .join()
        .expect("set_bootstrap_provider thread panicked");
    }

    extern "C" fn drop_fn<T: Source + 'static>(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut SourceWrapper<T>)) };
    }

    let cached_id = source.id().to_string();
    let cached_type_name = source.type_name().to_string();

    let wrapper = Box::new(SourceWrapper {
        inner: source,
        cached_id,
        cached_type_name,
        lifecycle_emitter,
        runtime_handle: runtime,
        vtable_executor: executor,
        instance_log_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_log_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_id: std::sync::RwLock::new(String::new()),
        _status_rx: std::sync::Mutex::new(None),
    });

    SourceVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        id_fn: id_fn::<T>,
        type_name_fn: type_name_fn::<T>,
        auto_start_fn: auto_start_fn::<T>,
        dispatch_mode_fn: dispatch_mode_fn::<T>,
        start_fn: start_fn::<T>,
        stop_fn: stop_fn::<T>,
        status_fn: status_fn::<T>,
        deprovision_fn: deprovision_fn::<T>,
        initialize_fn: initialize_fn::<T>,
        subscribe_fn: subscribe_fn::<T>,
        set_bootstrap_provider_fn: set_bootstrap_provider_fn::<T>,
        drop_fn: drop_fn::<T>,
    }
}

// ============================================================================
// Source vtable from Box<dyn Source> (returned by plugin factory)
// ============================================================================

/// Build a SourceVtable from a `Box<dyn Source>` (returned by plugin factory).
pub fn build_source_vtable_from_boxed(
    source: Box<dyn Source>,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime: fn() -> &'static tokio::runtime::Runtime,
) -> SourceVtable {
    struct DynSourceWrapper {
        inner: Box<dyn Source>,
        cached_id: String,
        cached_type_name: String,
        lifecycle_emitter: LifecycleEmitterFn,
        runtime_handle: fn() -> &'static tokio::runtime::Runtime,
        vtable_executor: AsyncExecutorFn,
        instance_log_cb: std::sync::atomic::AtomicPtr<()>,
        instance_log_ctx: std::sync::atomic::AtomicPtr<c_void>,
        instance_lifecycle_cb: std::sync::atomic::AtomicPtr<()>,
        instance_lifecycle_ctx: std::sync::atomic::AtomicPtr<c_void>,
        instance_id: std::sync::RwLock<String>,
        _status_rx: std::sync::Mutex<Option<ComponentEventReceiver>>,
    }

    extern "C" fn id_fn(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        FfiStr::from_str(&w.cached_id)
    }

    extern "C" fn type_name_fn(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        FfiStr::from_str(&w.cached_type_name)
    }

    extern "C" fn auto_start_fn(state: *const c_void) -> bool {
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        w.inner.auto_start()
    }

    extern "C" fn dispatch_mode_fn(state: *const c_void) -> FfiDispatchMode {
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        dispatch_mode_to_ffi(w.inner.dispatch_mode())
    }

    fn emit_dyn_source_lifecycle(
        w: &DynSourceWrapper,
        event_type: FfiLifecycleEventType,
        message: &str,
    ) {
        let cb_ptr = w
            .instance_lifecycle_cb
            .load(std::sync::atomic::Ordering::Acquire);
        if !cb_ptr.is_null() {
            let cb: super::callbacks::LifecycleCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
            let ctx = w
                .instance_lifecycle_ctx
                .load(std::sync::atomic::Ordering::Acquire);
            let event = FfiLifecycleEvent {
                component_id: FfiStr::from_str(&w.cached_id),
                component_type: FfiStr::from_str("source"),
                event_type,
                message: FfiStr::from_str(message),
                timestamp_us: super::now_us(),
            };
            cb(ctx, &event);
        } else {
            (w.lifecycle_emitter)(&w.cached_id, event_type, message);
        }
    }

    fn build_dyn_source_log_ctx(w: &DynSourceWrapper) -> Option<InstanceLogContext> {
        let cb_ptr = w.instance_log_cb.load(std::sync::atomic::Ordering::Acquire);
        if cb_ptr.is_null() {
            return None;
        }
        let cb: super::callbacks::LogCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
        let ctx = w
            .instance_log_ctx
            .load(std::sync::atomic::Ordering::Acquire);
        let instance_id = w.instance_id.read().map(|s| s.clone()).unwrap_or_default();
        Some(InstanceLogContext {
            instance_id,
            component_id: w.cached_id.clone(),
            component_type: "source".to_string(),
            log_cb: Some(cb),
            log_ctx: ctx,
        })
    }

    extern "C" fn start_fn(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const DynSourceWrapper) };
            emit_dyn_source_lifecycle(w, FfiLifecycleEventType::Starting, "");
            let log_ctx = build_dyn_source_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner_ptr = SendPtr(state as *const DynSourceWrapper);
                move || {
                    let inner = unsafe { inner_ptr.as_ref() };
                    with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.start()))
                }
            })
            .join()
            .expect("source start thread panicked");
            match result {
                Ok(()) => {
                    emit_dyn_source_lifecycle(w, FfiLifecycleEventType::Started, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_dyn_source_lifecycle(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn stop_fn(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const DynSourceWrapper) };
            emit_dyn_source_lifecycle(w, FfiLifecycleEventType::Stopping, "");
            let log_ctx = build_dyn_source_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner_ptr = SendPtr(state as *const DynSourceWrapper);
                move || {
                    let inner = unsafe { inner_ptr.as_ref() };
                    with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.stop()))
                }
            })
            .join()
            .expect("source stop thread panicked");
            match result {
                Ok(()) => {
                    emit_dyn_source_lifecycle(w, FfiLifecycleEventType::Stopped, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_dyn_source_lifecycle(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn status_fn(state: *const c_void) -> FfiComponentStatus {
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        let handle = (w.runtime_handle)().handle().clone();
        let status = std::thread::spawn({
            let inner_ptr = SendPtr(state as *const DynSourceWrapper);
            move || {
                let inner = unsafe { inner_ptr.as_ref() };
                handle.block_on(inner.inner.status())
            }
        })
        .join()
        .expect("source status thread panicked");
        component_status_to_ffi(status)
    }

    extern "C" fn deprovision_fn(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const DynSourceWrapper) };
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner_ptr = SendPtr(state as *const DynSourceWrapper);
                move || {
                    let inner = unsafe { inner_ptr.as_ref() };
                    handle.block_on(inner.inner.deprovision())
                }
            })
            .join()
            .expect("source deprovision thread panicked");
            match result {
                Ok(()) => FfiResult::ok(),
                Err(e) => FfiResult::err(e.to_string()),
            }
        })
    }

    extern "C" fn subscribe_fn(
        state: *mut c_void,
        source_id: FfiStr,
        enable_bootstrap: bool,
        query_id: FfiStr,
        nodes_json: FfiStr,
        relations_json: FfiStr,
    ) -> *mut FfiSubscriptionResponse {
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        let source_id_str = unsafe { source_id.to_string() };
        let qid = unsafe { query_id.to_string() };
        let nodes_str = unsafe { nodes_json.to_string() };
        let rels_str = unsafe { relations_json.to_string() };

        let nodes: HashSet<String> = serde_json::from_str(&nodes_str).unwrap_or_default();
        let relations: HashSet<String> = serde_json::from_str(&rels_str).unwrap_or_default();

        let settings = SourceSubscriptionSettings {
            source_id: source_id_str,
            enable_bootstrap,
            query_id: qid,
            nodes,
            relations,
        };

        let handle = (w.runtime_handle)().handle().clone();
        let result = std::thread::spawn({
            let inner_ptr = SendPtr(state as *const DynSourceWrapper);
            move || {
                let inner = unsafe { inner_ptr.as_ref() };
                handle.block_on(inner.inner.subscribe(settings))
            }
        })
        .join()
        .expect("source subscribe thread panicked");

        match result {
            Ok(sub) => {
                let executor = unsafe { &*(state as *const DynSourceWrapper) }.vtable_executor;
                wrap_subscription_response(sub, executor)
            }
            Err(e) => {
                log::error!("Subscribe failed: {}", e);
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn initialize_fn(state: *mut c_void, ctx: *const FfiRuntimeContext) {
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        let ffi_ctx = unsafe { &*ctx };

        if let Some(log_cb) = ffi_ctx.log_callback {
            w.instance_log_cb
                .store(log_cb as *mut (), std::sync::atomic::Ordering::Release);
            w.instance_log_ctx
                .store(ffi_ctx.log_ctx, std::sync::atomic::Ordering::Release);
        }
        if let Some(lifecycle_cb) = ffi_ctx.lifecycle_callback {
            w.instance_lifecycle_cb.store(
                lifecycle_cb as *mut (),
                std::sync::atomic::Ordering::Release,
            );
            w.instance_lifecycle_ctx
                .store(ffi_ctx.lifecycle_ctx, std::sync::atomic::Ordering::Release);
        }
        if let Ok(mut iid) = w.instance_id.write() {
            *iid = unsafe { ffi_ctx.instance_id.to_string() };
        }

        let (runtime_ctx, status_rx) = build_source_runtime_context(ffi_ctx);
        if let Ok(mut guard) = w._status_rx.lock() {
            *guard = Some(status_rx);
        }
        let handle = (w.runtime_handle)().handle().clone();
        std::thread::spawn({
            let inner_ptr = SendPtr(state as *const DynSourceWrapper);
            move || {
                let inner = unsafe { inner_ptr.as_ref() };
                handle.block_on(inner.inner.initialize(runtime_ctx))
            }
        })
        .join()
        .expect("source initialize thread panicked");
    }

    extern "C" fn set_bootstrap_provider_fn(
        state: *mut c_void,
        provider: *mut BootstrapProviderVtable,
    ) {
        if provider.is_null() {
            return;
        }
        let vtable = unsafe { Box::from_raw(provider) };
        let proxy = Box::new(FfiBootstrapProviderProxy {
            vtable: std::sync::Mutex::new(*vtable),
        });
        let w = unsafe { &*(state as *const DynSourceWrapper) };
        let handle = (w.runtime_handle)().handle().clone();
        std::thread::spawn({
            let inner_ptr = SendPtr(state as *const DynSourceWrapper);
            move || {
                let inner = unsafe { inner_ptr.as_ref() };
                handle.block_on(inner.inner.set_bootstrap_provider(proxy))
            }
        })
        .join()
        .expect("set_bootstrap_provider thread panicked");
    }

    extern "C" fn drop_fn(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut DynSourceWrapper)) };
    }

    let cached_id = source.id().to_string();
    let cached_type_name = source.type_name().to_string();

    let wrapper = Box::new(DynSourceWrapper {
        inner: source,
        cached_id,
        cached_type_name,
        lifecycle_emitter,
        runtime_handle: runtime,
        vtable_executor: executor,
        instance_log_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_log_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_id: std::sync::RwLock::new(String::new()),
        _status_rx: std::sync::Mutex::new(None),
    });

    SourceVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        id_fn,
        type_name_fn,
        auto_start_fn,
        dispatch_mode_fn,
        start_fn,
        stop_fn,
        status_fn,
        deprovision_fn,
        initialize_fn,
        subscribe_fn,
        set_bootstrap_provider_fn,
        drop_fn,
    }
}

// ============================================================================
// Reaction vtable generation
// ============================================================================

/// Internal wrapper holding a Reaction impl + runtime handle + lifecycle emitter.
pub(crate) struct ReactionWrapper<T: Reaction + 'static> {
    pub inner: T,
    pub cached_id: String,
    pub cached_type_name: String,
    pub lifecycle_emitter: LifecycleEmitterFn,
    pub runtime_handle: fn() -> &'static tokio::runtime::Runtime,
    pub instance_log_cb: std::sync::atomic::AtomicPtr<()>,
    pub instance_log_ctx: std::sync::atomic::AtomicPtr<c_void>,
    pub instance_lifecycle_cb: std::sync::atomic::AtomicPtr<()>,
    pub instance_lifecycle_ctx: std::sync::atomic::AtomicPtr<c_void>,
    pub instance_id: std::sync::RwLock<String>,
    pub _status_rx: std::sync::Mutex<Option<ComponentEventReceiver>>,
}

/// Build a ReactionVtable from a concrete type implementing the DrasiLib Reaction trait.
pub fn build_reaction_vtable<T: Reaction + 'static>(
    reaction: T,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime: fn() -> &'static tokio::runtime::Runtime,
) -> ReactionVtable {
    extern "C" fn id_fn<T: Reaction + 'static>(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const ReactionWrapper<T>) };
        FfiStr::from_str(&w.cached_id)
    }

    extern "C" fn type_name_fn<T: Reaction + 'static>(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const ReactionWrapper<T>) };
        FfiStr::from_str(&w.cached_type_name)
    }

    extern "C" fn auto_start_fn<T: Reaction + 'static>(state: *const c_void) -> bool {
        let w = unsafe { &*(state as *const ReactionWrapper<T>) };
        w.inner.auto_start()
    }

    extern "C" fn query_ids_fn<T: Reaction + 'static>(state: *const c_void) -> FfiStringArray {
        let w = unsafe { &*(state as *const ReactionWrapper<T>) };
        FfiStringArray::from_vec(w.inner.query_ids())
    }

    fn emit_reaction_lifecycle_for<T: Reaction + 'static>(
        w: &ReactionWrapper<T>,
        event_type: FfiLifecycleEventType,
        message: &str,
    ) {
        let cb_ptr = w
            .instance_lifecycle_cb
            .load(std::sync::atomic::Ordering::Acquire);
        if !cb_ptr.is_null() {
            let cb: super::callbacks::LifecycleCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
            let ctx = w
                .instance_lifecycle_ctx
                .load(std::sync::atomic::Ordering::Acquire);
            let event = FfiLifecycleEvent {
                component_id: FfiStr::from_str(&w.cached_id),
                component_type: FfiStr::from_str("reaction"),
                event_type,
                message: FfiStr::from_str(message),
                timestamp_us: super::now_us(),
            };
            cb(ctx, &event);
        } else {
            (w.lifecycle_emitter)(&w.cached_id, event_type, message);
        }
    }

    fn build_reaction_log_ctx<T: Reaction + 'static>(
        w: &ReactionWrapper<T>,
    ) -> Option<InstanceLogContext> {
        let cb_ptr = w.instance_log_cb.load(std::sync::atomic::Ordering::Acquire);
        if cb_ptr.is_null() {
            return None;
        }
        let cb: super::callbacks::LogCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
        let ctx = w
            .instance_log_ctx
            .load(std::sync::atomic::Ordering::Acquire);
        let instance_id = w.instance_id.read().map(|s| s.clone()).unwrap_or_default();
        Some(InstanceLogContext {
            instance_id,
            component_id: w.cached_id.clone(),
            component_type: "reaction".to_string(),
            log_cb: Some(cb),
            log_ctx: ctx,
        })
    }

    extern "C" fn start_fn<T: Reaction + 'static>(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const ReactionWrapper<T>) };
            emit_reaction_lifecycle_for(w, FfiLifecycleEventType::Starting, "");
            let log_ctx = build_reaction_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner = unsafe { &*(state as *const ReactionWrapper<T>) };
                move || with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.start()))
            })
            .join()
            .expect("reaction start thread panicked");
            match result {
                Ok(()) => {
                    emit_reaction_lifecycle_for(w, FfiLifecycleEventType::Started, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_reaction_lifecycle_for(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn stop_fn<T: Reaction + 'static>(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const ReactionWrapper<T>) };
            emit_reaction_lifecycle_for(w, FfiLifecycleEventType::Stopping, "");
            let log_ctx = build_reaction_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner = unsafe { &*(state as *const ReactionWrapper<T>) };
                move || with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.stop()))
            })
            .join()
            .expect("reaction stop thread panicked");
            match result {
                Ok(()) => {
                    emit_reaction_lifecycle_for(w, FfiLifecycleEventType::Stopped, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_reaction_lifecycle_for(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn status_fn<T: Reaction + 'static>(state: *const c_void) -> FfiComponentStatus {
        let w = unsafe { &*(state as *const ReactionWrapper<T>) };
        let handle = (w.runtime_handle)().handle().clone();
        let status = std::thread::spawn({
            let inner = unsafe { &*(state as *const ReactionWrapper<T>) };
            move || handle.block_on(inner.inner.status())
        })
        .join()
        .expect("reaction status thread panicked");
        component_status_to_ffi(status)
    }

    extern "C" fn deprovision_fn<T: Reaction + 'static>(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const ReactionWrapper<T>) };
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner = unsafe { &*(state as *const ReactionWrapper<T>) };
                move || handle.block_on(inner.inner.deprovision())
            })
            .join()
            .expect("reaction deprovision thread panicked");
            match result {
                Ok(()) => FfiResult::ok(),
                Err(e) => FfiResult::err(e.to_string()),
            }
        })
    }

    extern "C" fn initialize_fn<T: Reaction + 'static>(
        state: *mut c_void,
        ctx: *const FfiRuntimeContext,
    ) {
        let w = unsafe { &*(state as *const ReactionWrapper<T>) };
        let ffi_ctx = unsafe { &*ctx };

        // Capture per-instance callbacks
        if let Some(log_cb) = ffi_ctx.log_callback {
            w.instance_log_cb
                .store(log_cb as *mut (), std::sync::atomic::Ordering::Release);
            w.instance_log_ctx
                .store(ffi_ctx.log_ctx, std::sync::atomic::Ordering::Release);
        }
        if let Some(lifecycle_cb) = ffi_ctx.lifecycle_callback {
            w.instance_lifecycle_cb.store(
                lifecycle_cb as *mut (),
                std::sync::atomic::Ordering::Release,
            );
            w.instance_lifecycle_ctx
                .store(ffi_ctx.lifecycle_ctx, std::sync::atomic::Ordering::Release);
        }
        if let Ok(mut iid) = w.instance_id.write() {
            *iid = unsafe { ffi_ctx.instance_id.to_string() };
        }

        let (runtime_ctx, status_rx) = build_reaction_runtime_context(ffi_ctx);
        if let Ok(mut guard) = w._status_rx.lock() {
            *guard = Some(status_rx);
        }
        let handle = (w.runtime_handle)().handle().clone();
        std::thread::spawn({
            let inner = unsafe { &*(state as *const ReactionWrapper<T>) };
            move || handle.block_on(inner.inner.initialize(runtime_ctx))
        })
        .join()
        .expect("reaction initialize thread panicked");
    }

    extern "C" fn drop_fn<T: Reaction + 'static>(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut ReactionWrapper<T>)) };
    }

    let cached_id = reaction.id().to_string();
    let cached_type_name = reaction.type_name().to_string();

    let wrapper = Box::new(ReactionWrapper {
        inner: reaction,
        cached_id,
        cached_type_name,
        lifecycle_emitter,
        runtime_handle: runtime,
        instance_log_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_log_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_id: std::sync::RwLock::new(String::new()),
        _status_rx: std::sync::Mutex::new(None),
    });

    ReactionVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        id_fn: id_fn::<T>,
        type_name_fn: type_name_fn::<T>,
        auto_start_fn: auto_start_fn::<T>,
        query_ids_fn: query_ids_fn::<T>,
        start_fn: start_fn::<T>,
        stop_fn: stop_fn::<T>,
        status_fn: status_fn::<T>,
        deprovision_fn: deprovision_fn::<T>,
        initialize_fn: initialize_fn::<T>,
        drop_fn: drop_fn::<T>,
    }
}

/// Build a ReactionVtable from a `Box<dyn Reaction>`.
pub fn build_reaction_vtable_from_boxed(
    reaction: Box<dyn Reaction>,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime: fn() -> &'static tokio::runtime::Runtime,
) -> ReactionVtable {
    struct DynReactionWrapper {
        inner: Box<dyn Reaction>,
        cached_id: String,
        cached_type_name: String,
        lifecycle_emitter: LifecycleEmitterFn,
        runtime_handle: fn() -> &'static tokio::runtime::Runtime,
        instance_log_cb: std::sync::atomic::AtomicPtr<()>,
        instance_log_ctx: std::sync::atomic::AtomicPtr<c_void>,
        instance_lifecycle_cb: std::sync::atomic::AtomicPtr<()>,
        instance_lifecycle_ctx: std::sync::atomic::AtomicPtr<c_void>,
        instance_id: std::sync::RwLock<String>,
        _status_rx: std::sync::Mutex<Option<ComponentEventReceiver>>,
    }

    extern "C" fn id_fn(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const DynReactionWrapper) };
        FfiStr::from_str(&w.cached_id)
    }

    extern "C" fn type_name_fn(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const DynReactionWrapper) };
        FfiStr::from_str(&w.cached_type_name)
    }

    extern "C" fn auto_start_fn(state: *const c_void) -> bool {
        let w = unsafe { &*(state as *const DynReactionWrapper) };
        w.inner.auto_start()
    }

    extern "C" fn query_ids_fn(state: *const c_void) -> FfiStringArray {
        let w = unsafe { &*(state as *const DynReactionWrapper) };
        FfiStringArray::from_vec(w.inner.query_ids())
    }

    fn emit_dyn_reaction_lifecycle(
        w: &DynReactionWrapper,
        event_type: FfiLifecycleEventType,
        message: &str,
    ) {
        let cb_ptr = w
            .instance_lifecycle_cb
            .load(std::sync::atomic::Ordering::Acquire);
        if !cb_ptr.is_null() {
            let cb: super::callbacks::LifecycleCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
            let ctx = w
                .instance_lifecycle_ctx
                .load(std::sync::atomic::Ordering::Acquire);
            let event = FfiLifecycleEvent {
                component_id: FfiStr::from_str(&w.cached_id),
                component_type: FfiStr::from_str("reaction"),
                event_type,
                message: FfiStr::from_str(message),
                timestamp_us: super::now_us(),
            };
            cb(ctx, &event);
        } else {
            (w.lifecycle_emitter)(&w.cached_id, event_type, message);
        }
    }

    fn build_dyn_reaction_log_ctx(w: &DynReactionWrapper) -> Option<InstanceLogContext> {
        let cb_ptr = w.instance_log_cb.load(std::sync::atomic::Ordering::Acquire);
        if cb_ptr.is_null() {
            return None;
        }
        let cb: super::callbacks::LogCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
        let ctx = w
            .instance_log_ctx
            .load(std::sync::atomic::Ordering::Acquire);
        let instance_id = w.instance_id.read().map(|s| s.clone()).unwrap_or_default();
        Some(InstanceLogContext {
            instance_id,
            component_id: w.cached_id.clone(),
            component_type: "reaction".to_string(),
            log_cb: Some(cb),
            log_ctx: ctx,
        })
    }

    extern "C" fn start_fn(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const DynReactionWrapper) };
            emit_dyn_reaction_lifecycle(w, FfiLifecycleEventType::Starting, "");
            let log_ctx = build_dyn_reaction_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner_ptr = SendPtr(state as *const DynReactionWrapper);
                move || {
                    let inner = unsafe { inner_ptr.as_ref() };
                    with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.start()))
                }
            })
            .join()
            .expect("reaction start thread panicked");
            match result {
                Ok(()) => {
                    emit_dyn_reaction_lifecycle(w, FfiLifecycleEventType::Started, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_dyn_reaction_lifecycle(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn stop_fn(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const DynReactionWrapper) };
            emit_dyn_reaction_lifecycle(w, FfiLifecycleEventType::Stopping, "");
            let log_ctx = build_dyn_reaction_log_ctx(w);
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner_ptr = SendPtr(state as *const DynReactionWrapper);
                move || {
                    let inner = unsafe { inner_ptr.as_ref() };
                    with_instance_log_ctx(log_ctx, || handle.block_on(inner.inner.stop()))
                }
            })
            .join()
            .expect("reaction stop thread panicked");
            match result {
                Ok(()) => {
                    emit_dyn_reaction_lifecycle(w, FfiLifecycleEventType::Stopped, "");
                    FfiResult::ok()
                }
                Err(e) => {
                    let msg = e.to_string();
                    emit_dyn_reaction_lifecycle(w, FfiLifecycleEventType::Error, &msg);
                    FfiResult::err(msg)
                }
            }
        })
    }

    extern "C" fn status_fn(state: *const c_void) -> FfiComponentStatus {
        let w = unsafe { &*(state as *const DynReactionWrapper) };
        let handle = (w.runtime_handle)().handle().clone();
        let status = std::thread::spawn({
            let inner_ptr = SendPtr(state as *const DynReactionWrapper);
            move || {
                let inner = unsafe { inner_ptr.as_ref() };
                handle.block_on(inner.inner.status())
            }
        })
        .join()
        .expect("reaction status thread panicked");
        component_status_to_ffi(status)
    }

    extern "C" fn deprovision_fn(state: *mut c_void) -> FfiResult {
        catch_panic_ffi(|| {
            let w = unsafe { &*(state as *const DynReactionWrapper) };
            let handle = (w.runtime_handle)().handle().clone();
            let result = std::thread::spawn({
                let inner_ptr = SendPtr(state as *const DynReactionWrapper);
                move || {
                    let inner = unsafe { inner_ptr.as_ref() };
                    handle.block_on(inner.inner.deprovision())
                }
            })
            .join()
            .expect("reaction deprovision thread panicked");
            match result {
                Ok(()) => FfiResult::ok(),
                Err(e) => FfiResult::err(e.to_string()),
            }
        })
    }

    extern "C" fn initialize_fn(state: *mut c_void, ctx: *const FfiRuntimeContext) {
        let w = unsafe { &*(state as *const DynReactionWrapper) };
        let ffi_ctx = unsafe { &*ctx };

        if let Some(log_cb) = ffi_ctx.log_callback {
            w.instance_log_cb
                .store(log_cb as *mut (), std::sync::atomic::Ordering::Release);
            w.instance_log_ctx
                .store(ffi_ctx.log_ctx, std::sync::atomic::Ordering::Release);
        }
        if let Some(lifecycle_cb) = ffi_ctx.lifecycle_callback {
            w.instance_lifecycle_cb.store(
                lifecycle_cb as *mut (),
                std::sync::atomic::Ordering::Release,
            );
            w.instance_lifecycle_ctx
                .store(ffi_ctx.lifecycle_ctx, std::sync::atomic::Ordering::Release);
        }
        if let Ok(mut iid) = w.instance_id.write() {
            *iid = unsafe { ffi_ctx.instance_id.to_string() };
        }

        let (runtime_ctx, status_rx) = build_reaction_runtime_context(ffi_ctx);
        if let Ok(mut guard) = w._status_rx.lock() {
            *guard = Some(status_rx);
        }
        let handle = (w.runtime_handle)().handle().clone();
        std::thread::spawn({
            let inner_ptr = SendPtr(state as *const DynReactionWrapper);
            move || {
                let inner = unsafe { inner_ptr.as_ref() };
                handle.block_on(inner.inner.initialize(runtime_ctx))
            }
        })
        .join()
        .expect("reaction initialize thread panicked");
    }

    extern "C" fn drop_fn(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut DynReactionWrapper)) };
    }

    let cached_id = reaction.id().to_string();
    let cached_type_name = reaction.type_name().to_string();

    let wrapper = Box::new(DynReactionWrapper {
        inner: reaction,
        cached_id,
        cached_type_name,
        lifecycle_emitter,
        runtime_handle: runtime,
        instance_log_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_log_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_cb: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_lifecycle_ctx: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        instance_id: std::sync::RwLock::new(String::new()),
        _status_rx: std::sync::Mutex::new(None),
    });

    ReactionVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        id_fn,
        type_name_fn,
        auto_start_fn,
        query_ids_fn,
        start_fn,
        stop_fn,
        status_fn,
        deprovision_fn,
        initialize_fn,
        drop_fn,
    }
}

// ============================================================================
// Bootstrap provider vtable generation
// ============================================================================

/// Build a BootstrapProviderVtable from a `Box<dyn BootstrapProvider>`.
/// Used by the HOST to wrap a bootstrap provider into a vtable for passing to a source plugin.
pub fn build_bootstrap_provider_vtable(
    provider: Box<dyn BootstrapProvider>,
    executor: AsyncExecutorFn,
) -> BootstrapProviderVtable {
    struct BootstrapProviderWrapper {
        inner: Box<dyn BootstrapProvider>,
    }

    extern "C" fn bootstrap_fn(
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
    ) -> i64 {
        use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest};

        let query_id_str = unsafe { query_id.to_string() };
        let node_label_strs: Vec<String> = (0..node_labels_count)
            .map(|i| unsafe { (*node_labels.add(i)).to_string() })
            .collect();
        let rel_label_strs: Vec<String> = (0..relation_labels_count)
            .map(|i| unsafe { (*relation_labels.add(i)).to_string() })
            .collect();
        let request_id_str = unsafe { request_id.to_string() };
        let server_id_str = unsafe { server_id.to_string() };
        let source_id_str = unsafe { source_id.to_string() };

        let request = BootstrapRequest {
            query_id: query_id_str,
            node_labels: node_label_strs,
            relation_labels: rel_label_strs,
            request_id: request_id_str,
        };
        let context = BootstrapContext::new_minimal(server_id_str, source_id_str.clone());

        let ffi_sender = unsafe { &*sender };
        let send_fn = ffi_sender.send_fn;
        let sender_state = ffi_sender.state;

        // Use std::sync::mpsc so we can recv without async context
        let (std_tx, std_rx) = std::sync::mpsc::channel::<BootstrapEvent>();
        let (tokio_tx, mut tokio_rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(100);

        // Run the actual bootstrap provider in a background thread with its own tokio runtime
        let provider_ptr = SendPtr(state as *const BootstrapProviderWrapper);
        let _bootstrap_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build bootstrap runtime");
            let inner = unsafe { provider_ptr.as_ref() };
            rt.block_on(async {
                let forward_handle = tokio::spawn(async move {
                    while let Some(record) = tokio_rx.recv().await {
                        if std_tx.send(record).is_err() {
                            break;
                        }
                    }
                });
                let _ = inner
                    .inner
                    .bootstrap(request, &context, tokio_tx, None)
                    .await;
                let _ = forward_handle.await;
            })
        });

        // Forward records from std::sync::mpsc to FFI sender
        let mut count: usize = 0;
        while let Ok(record) = std_rx.recv() {
            let source_id_owned = record.source_id.clone();
            let timestamp_us = record
                .timestamp
                .timestamp_nanos_opt()
                .map(|n| n / 1000)
                .unwrap_or(0);
            let sequence = record.sequence;

            // Extract entity metadata for FFI envelope
            let entity_id_str = source_change_metadata(&record.change)
                .map(|m| m.reference.element_id.to_string())
                .unwrap_or_default();
            let label_str = source_change_metadata(&record.change)
                .and_then(|m| m.labels.first().map(|l| l.to_string()))
                .unwrap_or_default();

            let boxed = Box::new(record);
            let ptr = Box::into_raw(boxed);
            let event = Box::new(FfiBootstrapEvent {
                opaque: ptr as *mut c_void,
                source_id: FfiStr::from_str(&source_id_owned),
                timestamp_us,
                sequence,
                label: FfiStr::from_str(&label_str),
                entity_id: FfiStr::from_str(&entity_id_str),
                drop_fn: bootstrap_event_drop,
            });
            let event_ptr = Box::into_raw(event);
            let result = (send_fn)(sender_state, event_ptr);
            if result != 0 {
                break;
            }
            count += 1;
        }

        count as i64
    }

    extern "C" fn bootstrap_event_drop(opaque: *mut c_void) {
        if !opaque.is_null() {
            unsafe { drop(Box::from_raw(opaque as *mut BootstrapEvent)) };
        }
    }

    extern "C" fn drop_fn(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut BootstrapProviderWrapper)) };
    }

    let wrapper = Box::new(BootstrapProviderWrapper { inner: provider });
    BootstrapProviderVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        bootstrap_fn,
        drop_fn,
    }
}

// ============================================================================
// Source plugin descriptor vtable generation
// ============================================================================

struct SourcePluginWrapper<T: SourcePluginDescriptor + 'static> {
    inner: T,
    cached_kind: String,
    cached_config_version: String,
    cached_schema_name: String,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime_handle: fn() -> &'static tokio::runtime::Runtime,
}

/// Build a `SourcePluginVtable` from a type implementing `SourcePluginDescriptor`.
pub fn build_source_plugin_vtable<T: SourcePluginDescriptor + 'static>(
    descriptor: T,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime: fn() -> &'static tokio::runtime::Runtime,
) -> SourcePluginVtable {
    extern "C" fn kind_fn<T: SourcePluginDescriptor + 'static>(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const SourcePluginWrapper<T>) };
        FfiStr::from_str(&w.cached_kind)
    }

    extern "C" fn config_version_fn<T: SourcePluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiStr {
        let w = unsafe { &*(state as *const SourcePluginWrapper<T>) };
        FfiStr::from_str(&w.cached_config_version)
    }

    extern "C" fn config_schema_json_fn<T: SourcePluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiOwnedStr {
        let w = unsafe { &*(state as *const SourcePluginWrapper<T>) };
        FfiOwnedStr::from_string(w.inner.config_schema_json())
    }

    extern "C" fn config_schema_name_fn<T: SourcePluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiStr {
        let w = unsafe { &*(state as *const SourcePluginWrapper<T>) };
        FfiStr::from_str(&w.cached_schema_name)
    }

    extern "C" fn create_source_fn<T: SourcePluginDescriptor + 'static>(
        state: *mut c_void,
        id: FfiStr,
        config_json: FfiStr,
        auto_start: bool,
    ) -> *mut SourceVtable {
        let w = unsafe { &*(state as *const SourcePluginWrapper<T>) };
        let id_str = unsafe { id.to_string() };
        let config_str = unsafe { config_json.to_string() };

        let config_value: serde_json::Value = match serde_json::from_str(&config_str) {
            Ok(v) => v,
            Err(e) => {
                log::error!("Failed to parse config JSON for source '{}': {}", id_str, e);
                return std::ptr::null_mut();
            }
        };

        let handle = (w.runtime_handle)().handle().clone();
        let result = std::thread::spawn({
            let inner = unsafe { &*(state as *const SourcePluginWrapper<T>) };
            let id_owned = id_str.clone();
            move || {
                handle.block_on(
                    inner
                        .inner
                        .create_source(&id_owned, &config_value, auto_start),
                )
            }
        })
        .join()
        .expect("create_source thread panicked");

        match result {
            Ok(source) => {
                let vtable = build_source_vtable_from_boxed(
                    source,
                    w.executor,
                    w.lifecycle_emitter,
                    w.runtime_handle,
                );
                Box::into_raw(Box::new(vtable))
            }
            Err(e) => {
                log::error!("Failed to create source '{}': {}", id_str, e);
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn drop_fn<T: SourcePluginDescriptor + 'static>(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut SourcePluginWrapper<T>)) };
    }

    let cached_kind = descriptor.kind().to_string();
    let cached_config_version = descriptor.config_version().to_string();
    let cached_schema_name = descriptor.config_schema_name().to_string();

    let wrapper = Box::new(SourcePluginWrapper {
        inner: descriptor,
        cached_kind,
        cached_config_version,
        cached_schema_name,
        executor,
        lifecycle_emitter,
        runtime_handle: runtime,
    });

    SourcePluginVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        kind_fn: kind_fn::<T>,
        config_version_fn: config_version_fn::<T>,
        config_schema_json_fn: config_schema_json_fn::<T>,
        config_schema_name_fn: config_schema_name_fn::<T>,
        create_source_fn: create_source_fn::<T>,
        drop_fn: drop_fn::<T>,
    }
}

// ============================================================================
// Reaction plugin descriptor vtable generation
// ============================================================================

struct ReactionPluginWrapper<T: ReactionPluginDescriptor + 'static> {
    inner: T,
    cached_kind: String,
    cached_config_version: String,
    cached_schema_name: String,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime_handle: fn() -> &'static tokio::runtime::Runtime,
}

/// Build a `ReactionPluginVtable` from a type implementing `ReactionPluginDescriptor`.
pub fn build_reaction_plugin_vtable<T: ReactionPluginDescriptor + 'static>(
    descriptor: T,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime: fn() -> &'static tokio::runtime::Runtime,
) -> ReactionPluginVtable {
    extern "C" fn kind_fn<T: ReactionPluginDescriptor + 'static>(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const ReactionPluginWrapper<T>) };
        FfiStr::from_str(&w.cached_kind)
    }

    extern "C" fn config_version_fn<T: ReactionPluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiStr {
        let w = unsafe { &*(state as *const ReactionPluginWrapper<T>) };
        FfiStr::from_str(&w.cached_config_version)
    }

    extern "C" fn config_schema_json_fn<T: ReactionPluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiOwnedStr {
        let w = unsafe { &*(state as *const ReactionPluginWrapper<T>) };
        FfiOwnedStr::from_string(w.inner.config_schema_json())
    }

    extern "C" fn config_schema_name_fn<T: ReactionPluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiStr {
        let w = unsafe { &*(state as *const ReactionPluginWrapper<T>) };
        FfiStr::from_str(&w.cached_schema_name)
    }

    extern "C" fn create_reaction_fn<T: ReactionPluginDescriptor + 'static>(
        state: *mut c_void,
        id: FfiStr,
        query_ids_json: FfiStr,
        config_json: FfiStr,
        auto_start: bool,
    ) -> *mut ReactionVtable {
        let w = unsafe { &*(state as *const ReactionPluginWrapper<T>) };
        let id_str = unsafe { id.to_string() };
        let query_ids_str = unsafe { query_ids_json.to_string() };
        let config_str = unsafe { config_json.to_string() };

        let query_ids: Vec<String> = match serde_json::from_str(&query_ids_str) {
            Ok(v) => v,
            Err(e) => {
                log::error!(
                    "Failed to parse query_ids JSON for reaction '{}': {}",
                    id_str,
                    e
                );
                return std::ptr::null_mut();
            }
        };

        let config_value: serde_json::Value = match serde_json::from_str(&config_str) {
            Ok(v) => v,
            Err(e) => {
                log::error!(
                    "Failed to parse config JSON for reaction '{}': {}",
                    id_str,
                    e
                );
                return std::ptr::null_mut();
            }
        };

        let handle = (w.runtime_handle)().handle().clone();
        let result = std::thread::spawn({
            let inner = unsafe { &*(state as *const ReactionPluginWrapper<T>) };
            let id_owned = id_str.clone();
            move || {
                handle.block_on(inner.inner.create_reaction(
                    &id_owned,
                    query_ids,
                    &config_value,
                    auto_start,
                ))
            }
        })
        .join()
        .expect("create_reaction thread panicked");

        match result {
            Ok(reaction) => {
                let vtable = build_reaction_vtable_from_boxed(
                    reaction,
                    w.executor,
                    w.lifecycle_emitter,
                    w.runtime_handle,
                );
                Box::into_raw(Box::new(vtable))
            }
            Err(e) => {
                log::error!("Failed to create reaction '{}': {}", id_str, e);
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn drop_fn<T: ReactionPluginDescriptor + 'static>(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut ReactionPluginWrapper<T>)) };
    }

    let cached_kind = descriptor.kind().to_string();
    let cached_config_version = descriptor.config_version().to_string();
    let cached_schema_name = descriptor.config_schema_name().to_string();

    let wrapper = Box::new(ReactionPluginWrapper {
        inner: descriptor,
        cached_kind,
        cached_config_version,
        cached_schema_name,
        executor,
        lifecycle_emitter,
        runtime_handle: runtime,
    });

    ReactionPluginVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        kind_fn: kind_fn::<T>,
        config_version_fn: config_version_fn::<T>,
        config_schema_json_fn: config_schema_json_fn::<T>,
        config_schema_name_fn: config_schema_name_fn::<T>,
        create_reaction_fn: create_reaction_fn::<T>,
        drop_fn: drop_fn::<T>,
    }
}

// ============================================================================
// Bootstrap plugin descriptor vtable generation
// ============================================================================

struct BootstrapPluginWrapper<T: BootstrapPluginDescriptor + 'static> {
    inner: T,
    cached_kind: String,
    cached_config_version: String,
    cached_schema_name: String,
    executor: AsyncExecutorFn,
    #[allow(dead_code)]
    lifecycle_emitter: LifecycleEmitterFn,
    runtime_handle: fn() -> &'static tokio::runtime::Runtime,
}

/// Build a `BootstrapPluginVtable` from a type implementing `BootstrapPluginDescriptor`.
pub fn build_bootstrap_plugin_vtable<T: BootstrapPluginDescriptor + 'static>(
    descriptor: T,
    executor: AsyncExecutorFn,
    lifecycle_emitter: LifecycleEmitterFn,
    runtime: fn() -> &'static tokio::runtime::Runtime,
) -> BootstrapPluginVtable {
    extern "C" fn kind_fn<T: BootstrapPluginDescriptor + 'static>(state: *const c_void) -> FfiStr {
        let w = unsafe { &*(state as *const BootstrapPluginWrapper<T>) };
        FfiStr::from_str(&w.cached_kind)
    }

    extern "C" fn config_version_fn<T: BootstrapPluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiStr {
        let w = unsafe { &*(state as *const BootstrapPluginWrapper<T>) };
        FfiStr::from_str(&w.cached_config_version)
    }

    extern "C" fn config_schema_json_fn<T: BootstrapPluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiOwnedStr {
        let w = unsafe { &*(state as *const BootstrapPluginWrapper<T>) };
        FfiOwnedStr::from_string(w.inner.config_schema_json())
    }

    extern "C" fn config_schema_name_fn<T: BootstrapPluginDescriptor + 'static>(
        state: *const c_void,
    ) -> FfiStr {
        let w = unsafe { &*(state as *const BootstrapPluginWrapper<T>) };
        FfiStr::from_str(&w.cached_schema_name)
    }

    extern "C" fn create_bootstrap_provider_fn<T: BootstrapPluginDescriptor + 'static>(
        state: *mut c_void,
        config_json: FfiStr,
        source_config_json: FfiStr,
    ) -> *mut BootstrapProviderVtable {
        let w = unsafe { &*(state as *const BootstrapPluginWrapper<T>) };
        let config_str = unsafe { config_json.to_string() };
        let source_config_str = unsafe { source_config_json.to_string() };

        let config_value: serde_json::Value = match serde_json::from_str(&config_str) {
            Ok(v) => v,
            Err(e) => {
                log::error!("Failed to parse bootstrap config JSON: {}", e);
                return std::ptr::null_mut();
            }
        };
        let source_config_value: serde_json::Value = match serde_json::from_str(&source_config_str)
        {
            Ok(v) => v,
            Err(e) => {
                log::error!("Failed to parse source config JSON: {}", e);
                return std::ptr::null_mut();
            }
        };

        let handle = (w.runtime_handle)().handle().clone();
        let result = std::thread::spawn({
            let inner = unsafe { &*(state as *const BootstrapPluginWrapper<T>) };
            move || {
                handle.block_on(
                    inner
                        .inner
                        .create_bootstrap_provider(&config_value, &source_config_value),
                )
            }
        })
        .join()
        .expect("create_bootstrap_provider thread panicked");

        match result {
            Ok(provider) => {
                let vtable = build_bootstrap_provider_vtable(provider, w.executor);
                Box::into_raw(Box::new(vtable))
            }
            Err(e) => {
                log::error!("Failed to create bootstrap provider: {}", e);
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn drop_fn<T: BootstrapPluginDescriptor + 'static>(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut BootstrapPluginWrapper<T>)) };
    }

    let cached_kind = descriptor.kind().to_string();
    let cached_config_version = descriptor.config_version().to_string();
    let cached_schema_name = descriptor.config_schema_name().to_string();

    let wrapper = Box::new(BootstrapPluginWrapper {
        inner: descriptor,
        cached_kind,
        cached_config_version,
        cached_schema_name,
        executor,
        lifecycle_emitter,
        runtime_handle: runtime,
    });

    BootstrapPluginVtable {
        state: Box::into_raw(wrapper) as *mut c_void,
        executor,
        kind_fn: kind_fn::<T>,
        config_version_fn: config_version_fn::<T>,
        config_schema_json_fn: config_schema_json_fn::<T>,
        config_schema_name_fn: config_schema_name_fn::<T>,
        create_bootstrap_provider_fn: create_bootstrap_provider_fn::<T>,
        drop_fn: drop_fn::<T>,
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Build a SourceRuntimeContext from FFI runtime context.
fn build_source_runtime_context(
    ffi_ctx: &FfiRuntimeContext,
) -> (SourceRuntimeContext, ComponentEventReceiver) {
    let instance_id = unsafe { ffi_ctx.instance_id.to_string() };
    let component_id = unsafe { ffi_ctx.component_id.to_string() };
    let state_store: Option<Arc<dyn StateStoreProvider>> = if ffi_ctx.state_store.is_null() {
        None
    } else {
        Some(Arc::new(FfiStateStoreProxy {
            vtable: ffi_ctx.state_store,
        }))
    };
    let identity_provider: Option<Arc<dyn drasi_lib::identity::IdentityProvider>> =
        if ffi_ctx.identity_provider.is_null() {
            None
        } else {
            Some(Arc::new(unsafe {
                super::identity_proxy::FfiIdentityProviderProxy::new(ffi_ctx.identity_provider)
            }))
        };
    let (status_tx, status_rx) = tokio::sync::mpsc::channel(16);
    let ctx = SourceRuntimeContext {
        instance_id,
        source_id: component_id,
        status_tx,
        state_store,
        identity_provider,
    };
    (ctx, status_rx)
}

/// Build a ReactionRuntimeContext from FFI runtime context.
/// Note: query_provider is a stub since the host manages subscriptions for cdylib reactions.
fn build_reaction_runtime_context(
    ffi_ctx: &FfiRuntimeContext,
) -> (drasi_lib::ReactionRuntimeContext, ComponentEventReceiver) {
    let instance_id = unsafe { ffi_ctx.instance_id.to_string() };
    let component_id = unsafe { ffi_ctx.component_id.to_string() };
    let state_store: Option<Arc<dyn StateStoreProvider>> = if ffi_ctx.state_store.is_null() {
        None
    } else {
        Some(Arc::new(FfiStateStoreProxy {
            vtable: ffi_ctx.state_store,
        }))
    };
    let identity_provider: Option<Arc<dyn drasi_lib::identity::IdentityProvider>> =
        if ffi_ctx.identity_provider.is_null() {
            None
        } else {
            Some(Arc::new(unsafe {
                super::identity_proxy::FfiIdentityProviderProxy::new(ffi_ctx.identity_provider)
            }))
        };
    let (status_tx, status_rx) = tokio::sync::mpsc::channel(16);

    // Stub QueryProvider — the host manages query subscriptions for cdylib reactions
    struct StubQueryProvider;
    #[async_trait::async_trait]
    impl drasi_lib::QueryProvider for StubQueryProvider {
        async fn get_query_instance(
            &self,
            id: &str,
        ) -> anyhow::Result<Arc<dyn drasi_lib::queries::Query>> {
            anyhow::bail!(
                "QueryProvider not available in dynamic plugin mode. Query '{}' subscriptions are managed by the host.",
                id
            )
        }
    }

    let ctx = drasi_lib::ReactionRuntimeContext {
        instance_id,
        reaction_id: component_id,
        status_tx,
        state_store,
        query_provider: Arc::new(StubQueryProvider),
        identity_provider,
    };
    (ctx, status_rx)
}

/// Wrap a DrasiLib SubscriptionResponse into an FfiSubscriptionResponse.
fn wrap_subscription_response(
    sub: drasi_lib::SubscriptionResponse,
    executor: AsyncExecutorFn,
) -> *mut FfiSubscriptionResponse {
    use drasi_lib::channels::events::SourceEventWrapper;

    // Wrap ChangeReceiver<SourceEventWrapper> → FfiChangeReceiver
    struct DrasiLibChangeReceiver {
        inner: std::sync::Mutex<Box<dyn ChangeReceiver<SourceEventWrapper>>>,
    }

    extern "C" fn change_receiver_recv(state: *mut c_void) -> *mut FfiSourceEvent {
        let r = unsafe { &*(state as *const DrasiLibChangeReceiver) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let event = {
            let mut rx = r.inner.lock().unwrap();
            rt.block_on(rx.recv())
        };
        match event {
            Ok(wrapper) => {
                let op = match &wrapper.event {
                    drasi_lib::channels::events::SourceEvent::Change(change) => match change {
                        SourceChange::Insert { .. } => FfiChangeOp::Insert,
                        SourceChange::Update { .. } => FfiChangeOp::Update,
                        SourceChange::Delete { .. } => FfiChangeOp::Delete,
                        SourceChange::Future { .. } => FfiChangeOp::Update,
                    },
                    _ => FfiChangeOp::Update,
                };
                let source_id_str = wrapper.source_id.clone();
                let timestamp_us = wrapper
                    .timestamp
                    .timestamp_nanos_opt()
                    .map(|n| n / 1000)
                    .unwrap_or(0);
                let boxed = Box::new(Arc::try_unwrap(wrapper).unwrap_or_else(|arc| (*arc).clone()));
                let source_id = FfiStr::from_str(&boxed.source_id);
                let opaque = Box::into_raw(boxed) as *mut c_void;
                extern "C" fn drop_wrapper(ptr: *mut c_void) {
                    unsafe { drop(Box::from_raw(ptr as *mut SourceEventWrapper)) };
                }
                Box::into_raw(Box::new(FfiSourceEvent {
                    opaque,
                    source_id,
                    timestamp_us,
                    op,
                    label: FfiStr::from_str(""), // extracted by host if needed
                    entity_id: FfiStr::from_str(""),
                    drop_fn: drop_wrapper,
                }))
            }
            Err(_) => std::ptr::null_mut(),
        }
    }

    extern "C" fn change_receiver_drop(state: *mut c_void) {
        unsafe { drop(Box::from_raw(state as *mut DrasiLibChangeReceiver)) };
    }

    let ffi_receiver = Box::new(DrasiLibChangeReceiver {
        inner: std::sync::Mutex::new(sub.receiver),
    });
    let ffi_rx = Box::new(FfiChangeReceiver {
        state: Box::into_raw(ffi_receiver) as *mut c_void,
        executor,
        recv_fn: change_receiver_recv,
        drop_fn: change_receiver_drop,
    });

    // Wrap bootstrap receiver if present
    let bootstrap_receiver = if let Some(mut brx) = sub.bootstrap_receiver {
        struct DrasiLibBootstrapReceiver {
            inner: std::sync::Mutex<tokio::sync::mpsc::Receiver<BootstrapEvent>>,
        }

        extern "C" fn bootstrap_recv(state: *mut c_void) -> *mut FfiBootstrapEvent {
            let r = unsafe { &*(state as *const DrasiLibBootstrapReceiver) };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let event = {
                let mut rx = r.inner.lock().unwrap();
                rt.block_on(rx.recv())
            };
            match event {
                Some(record) => {
                    let source_id_owned = record.source_id.clone();
                    let timestamp_us = record
                        .timestamp
                        .timestamp_nanos_opt()
                        .map(|n| n / 1000)
                        .unwrap_or(0);
                    let sequence = record.sequence;
                    let entity_id_str = source_change_metadata(&record.change)
                        .map(|m| m.reference.element_id.to_string())
                        .unwrap_or_default();
                    let label_str = source_change_metadata(&record.change)
                        .and_then(|m| m.labels.first().map(|l| l.to_string()))
                        .unwrap_or_default();

                    let boxed = Box::new(record);
                    let opaque = Box::into_raw(boxed) as *mut c_void;
                    extern "C" fn drop_bootstrap(ptr: *mut c_void) {
                        unsafe { drop(Box::from_raw(ptr as *mut BootstrapEvent)) };
                    }
                    Box::into_raw(Box::new(FfiBootstrapEvent {
                        opaque,
                        source_id: FfiStr::from_str(&source_id_owned),
                        timestamp_us,
                        sequence,
                        label: FfiStr::from_str(&label_str),
                        entity_id: FfiStr::from_str(&entity_id_str),
                        drop_fn: drop_bootstrap,
                    }))
                }
                None => std::ptr::null_mut(),
            }
        }

        extern "C" fn bootstrap_drop(state: *mut c_void) {
            unsafe { drop(Box::from_raw(state as *mut DrasiLibBootstrapReceiver)) };
        }

        let ffi_brx = Box::new(DrasiLibBootstrapReceiver {
            inner: std::sync::Mutex::new(brx),
        });
        let ffi_brx_wrapper = Box::new(FfiBootstrapReceiver {
            state: Box::into_raw(ffi_brx) as *mut c_void,
            recv_fn: bootstrap_recv,
            drop_fn: bootstrap_drop,
        });
        Box::into_raw(ffi_brx_wrapper)
    } else {
        std::ptr::null_mut()
    };

    Box::into_raw(Box::new(FfiSubscriptionResponse {
        query_id: FfiOwnedStr::from_string(sub.query_id),
        source_id: FfiOwnedStr::from_string(sub.source_id),
        receiver: Box::into_raw(ffi_rx),
        bootstrap_receiver,
    }))
}
