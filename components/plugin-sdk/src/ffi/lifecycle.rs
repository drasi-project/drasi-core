// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Per-object guards for synchronous ordinary lifecycle callbacks.
//!
//! The graph owns lifecycle policy. These guards prevent a repeated FFI call
//! from repeating successful setup/cleanup or reactivating incomplete cleanup.

use std::sync::{Mutex, OnceLock};

use super::types::{catch_panic_ffi, FfiResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LifecycleOperation {
    Initialize,
    Start,
    Stop,
    Dispose,
    PauseProcessing,
    ResumeProcessing,
    DeleteOwnedResources,
}

#[derive(Default)]
struct Phase {
    bound: bool,
    initialized: bool,
    running: bool,
    needs_stop: bool,
    paused: bool,
    needs_resume: bool,
    disposing: bool,
    disposed: bool,
    deleting: bool,
    deleted: bool,
}

#[derive(Default)]
pub(crate) struct InitializationState {
    failure: OnceLock<String>,
    phase: Mutex<Phase>,
}

impl InitializationState {
    pub(super) fn failure(&self) -> Option<&str> {
        self.failure.get().map(String::as_str)
    }

    pub(super) fn bind<T>(
        &self,
        prepare: impl FnOnce() -> T,
        bind: impl FnOnce(T) -> FfiResult,
    ) -> Result<(), String> {
        let mut phase = self
            .phase
            .lock()
            .map_err(|error| format!("plugin lifecycle lock poisoned: {error}"))?;
        let result = catch_panic_ffi(std::panic::AssertUnwindSafe(|| {
            // Take ownership of transferred services even when rejecting reuse.
            let context = prepare();
            if let Some(error) = self.failure() {
                return FfiResult::err(error.to_owned());
            }
            if phase.disposing || phase.disposed {
                return FfiResult::err("cannot bind a retiring plugin instance".into());
            }
            if phase.bound {
                return FfiResult::ok();
            }
            bind(context)
        }));
        match unsafe { result.into_result() } {
            Ok(()) => {
                phase.bound = true;
                Ok(())
            }
            Err(error) => {
                let error = self.failure.get_or_init(|| {
                    format!("{error}; context binding failed: dispose and recreate this instance")
                });
                Err(error.clone())
            }
        }
    }

    pub(super) fn run(
        &self,
        operation: LifecycleOperation,
        callback: impl FnOnce() -> FfiResult,
    ) -> Result<(), String> {
        use LifecycleOperation::*;
        let mut phase = self
            .phase
            .lock()
            .map_err(|error| format!("plugin lifecycle lock poisoned: {error}"))?;

        if operation == Dispose && phase.disposed {
            return Ok(());
        }
        if phase.disposed || (phase.disposing && operation != Dispose) {
            return Err("plugin instance is retiring or disposed".into());
        }
        if phase.deleting && operation != DeleteOwnedResources {
            return Err("owned-resource deletion is incomplete; retry it before continuing".into());
        }
        match operation {
            Initialize | Start | PauseProcessing | ResumeProcessing => {
                if let Some(error) = self.failure() {
                    return Err(error.to_owned());
                }
                if !phase.bound {
                    return Err("runtime context must be bound before initialization".into());
                }
                if operation != Initialize && !phase.initialized {
                    return Err("plugin initialization has not completed".into());
                }
            }
            Stop | Dispose | DeleteOwnedResources => {}
        }
        match operation {
            Initialize if phase.initialized => return Ok(()),
            Start if phase.running => return Ok(()),
            Start if phase.needs_stop => {
                return Err("previous activation requires successful stop before restart".into());
            }
            Start if phase.deleted => {
                return Err("owned resources were deleted; retire this plugin instance".into());
            }
            Stop if !phase.needs_stop => return Ok(()),
            Dispose | DeleteOwnedResources if phase.needs_stop => {
                return Err("successful stop is required before instance cleanup".into());
            }
            DeleteOwnedResources if phase.deleted => return Ok(()),
            PauseProcessing | ResumeProcessing if !phase.running => {
                return Err("processing can only be paused/resumed in a running activation".into());
            }
            PauseProcessing if phase.paused => return Ok(()),
            ResumeProcessing if !phase.needs_resume => return Ok(()),
            _ => {}
        }

        match operation {
            // A failed start may already own work. Do not retry until stop succeeds.
            Start => phase.needs_stop = true,
            Stop => phase.running = false,
            Dispose => phase.disposing = true,
            DeleteOwnedResources => phase.deleting = true,
            PauseProcessing => phase.needs_resume = true,
            ResumeProcessing => phase.paused = false,
            _ => {}
        }
        let result =
            unsafe { catch_panic_ffi(std::panic::AssertUnwindSafe(callback)).into_result() };
        match &result {
            Ok(()) => match operation {
                Initialize => phase.initialized = true,
                Start => phase.running = true,
                Stop => {
                    phase.needs_stop = false;
                    phase.paused = false;
                    phase.needs_resume = false;
                }
                Dispose => phase.disposed = true,
                PauseProcessing => phase.paused = true,
                ResumeProcessing => phase.needs_resume = false,
                DeleteOwnedResources => {
                    phase.deleting = false;
                    phase.deleted = true;
                }
            },
            Err(error) if operation == Initialize => {
                self.failure.get_or_init(|| {
                    format!("{error}; initialization failed: dispose and recreate this instance")
                });
            }
            _ => {}
        }
        result
    }
}
