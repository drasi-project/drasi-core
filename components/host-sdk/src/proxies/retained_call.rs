// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Owns one ordinary plugin call across cancellation of its async waiter.
//!
//! Ordinary callbacks are synchronous and cannot be force-cancelled safely.
//! A timeout retains the blocking worker, its result and the actual plugin
//! allocation. Retrying joins that call before submitting any further work.

use drasi_lib::ComponentId;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context};
use drasi_computation_contracts::workers::{
    join_owned_worker_gracefully, spawn_owned_blocking_worker, WorkerCleanupError, WorkerCompletion,
};
use drasi_plugin_sdk::ffi::{FfiResult, SendMutPtr};
use libloading::Library;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::callbacks::InstanceCallbackContext;

const CALL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallKind {
    BindRuntimeContext,
    Initialize,
    Start,
    Stop,
    Dispose,
    PauseProcessing,
    ResumeProcessing,
    DeleteOwnedResources,
}

pub(super) struct PluginOwner {
    pub state: SendMutPtr<c_void>,
    drop_fn: extern "C" fn(*mut c_void),
    _library: Arc<Library>,
    pub callback_context: Mutex<Option<Arc<InstanceCallbackContext>>>,
    // Reaction callbacks borrow the host's forwarding state. Keep it alive
    // alongside the plugin even if the proxy is abandoned during cleanup.
    _forwarding: Option<Arc<dyn Send + Sync>>,
}

// Ordinary Source/Reaction implementations are Send + Sync. This pointer is
// stable until this owner is released; mutating lifecycle calls share one gate.
unsafe impl Sync for PluginOwner {}

impl Drop for PluginOwner {
    fn drop(&mut self) {
        super::drop_worker::execute_drop_fn(self.drop_fn, SendMutPtr(self.state.as_ptr()));
        // The raw Arc reference transferred to ordinary log/status callbacks is
        // intentionally process-pinned, as before; late plugin callbacks remain safe.
    }
}

struct CallState {
    pending: Option<CallKind>,
    binding_failure: Option<String>,
    release_safe: bool,
    disposed: bool,
}

pub(super) struct RetainedCall {
    owner: Option<Arc<PluginOwner>>,
    state: tokio::sync::Mutex<CallState>,
    worker: RwLock<Option<JoinHandle<anyhow::Result<()>>>>,
    wait_timeout: Duration,
    id: ComponentId,
}

impl RetainedCall {
    #[cfg(test)]
    pub fn set_wait_timeout(&mut self, timeout: Duration) {
        self.wait_timeout = timeout;
    }

    pub fn new(
        state: *mut c_void,
        drop_fn: extern "C" fn(*mut c_void),
        library: Arc<Library>,
        id: ComponentId,
        forwarding: Option<Arc<dyn Send + Sync>>,
    ) -> Self {
        Self {
            owner: Some(Arc::new(PluginOwner {
                state: SendMutPtr(state),
                drop_fn,
                _library: library,
                callback_context: Mutex::new(None),
                _forwarding: forwarding,
            })),
            state: tokio::sync::Mutex::new(CallState {
                pending: None,
                binding_failure: None,
                release_safe: false,
                disposed: false,
            }),
            worker: RwLock::new(None),
            wait_timeout: CALL_WAIT_TIMEOUT,
            id,
        }
    }

    pub async fn call(
        &self,
        kind: CallKind,
        callback: extern "C" fn(*mut c_void) -> FfiResult,
    ) -> anyhow::Result<()> {
        self.run(kind, move |owner| {
            unsafe { callback(owner.state.as_ptr()).into_result() }.map_err(anyhow::Error::msg)
        })
        .await
    }

    pub async fn run(
        &self,
        kind: CallKind,
        work: impl FnOnce(&PluginOwner) -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        if let Some(pending) = state.pending {
            let result = self.join(&mut state, pending).await;
            if pending == kind {
                // A retry observes the original result, not a second invocation.
                return result;
            }
            result.with_context(|| {
                format!("previous {pending:?} call failed; {kind:?} has not been submitted")
            })?;
        }
        anyhow::ensure!(
            !state.disposed || kind == CallKind::Dispose,
            "plugin '{}' has been disposed",
            self.id
        );
        if state.disposed {
            return Ok(());
        }
        if matches!(
            kind,
            CallKind::Initialize
                | CallKind::Start
                | CallKind::PauseProcessing
                | CallKind::ResumeProcessing
        ) {
            if let Some(error) = &state.binding_failure {
                return Err(anyhow!("runtime context binding failed: {error}"));
            }
        }
        let owner = self.owner.as_ref().expect("live plugin owner").clone();
        spawn_owned_blocking_worker(&self.worker, move || work(&owner)).await?;
        // No suspension between registration and recording the pending operation.
        state.pending = Some(kind);
        state.release_safe = false;
        self.join(&mut state, kind).await
    }

    async fn join(&self, state: &mut CallState, kind: CallKind) -> anyhow::Result<()> {
        let result =
            join_owned_worker_gracefully(&mut *self.worker.write().await, self.wait_timeout).await;
        if matches!(result, Err(WorkerCleanupError::TimedOut { .. })) {
            return Err(anyhow!(
                "plugin '{}' {kind:?} did not finish within {:?}; the synchronous callback cannot \
                 be force-cancelled safely, its worker and plugin owner are retained; retry to join it",
                self.id,
                self.wait_timeout
            ));
        }
        state.pending = None;
        let result = match result {
            Ok(WorkerCompletion::Completed(result)) => result,
            Ok(WorkerCompletion::Cancelled) => Err(anyhow!(
                "plugin '{}' {kind:?} worker was cancelled before completion; cleanup is not confirmed",
                self.id
            )),
            Ok(WorkerCompletion::Absent) => Err(anyhow!(
                "plugin '{}' lost its pending {kind:?} worker; cleanup is not confirmed",
                self.id
            )),
            Err(error) => Err(error.into()),
        };
        if kind == CallKind::BindRuntimeContext {
            state.binding_failure = result.as_ref().err().map(|error| format!("{error:#}"));
        }
        if kind == CallKind::Dispose && result.is_ok() {
            state.disposed = true;
            state.release_safe = true;
        }
        result
    }
}

impl Drop for RetainedCall {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        let worker = self.worker.get_mut().take();
        if !state.release_safe || worker.is_some() {
            log::error!(
                "Plugin '{}' dropped without confirmed disposal (pending: {:?}); retaining the \
                 actual plugin/library and any unjoined worker. Cleanup did not complete. \
                 Keep the proxy and retry cleanup instead of abandoning it.",
                self.id,
                state.pending
            );
            // Drop cannot await, and freeing the plugin or dropping a JoinHandle
            // would respectively race foreign code or detach unfinished work.
            // This is a last-resort ownership retention, not successful cleanup.
            // Normal and retry paths join the worker and dispose before Drop.
            let _retained = Box::leak(Box::new((self.owner.take(), worker)));
        }
    }
}
