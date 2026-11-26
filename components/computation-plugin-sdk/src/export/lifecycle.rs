// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! State held by the existing exported instance, not a second operation executor.
//! Accepting a mutable transition fences it before the owned future is polled:
//! dropping an operation never rolls a partial transition back into readiness.

use super::*;
use crate::{
    lifecycle::{DisposeReason, StateIntent},
    wire::LifecycleRequest,
    ApplyUpdateRequest, ComponentUpdateMode,
};
use std::{future::Future, sync::MutexGuard};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Initialization {
    New,
    Incomplete,
    Complete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Activation {
    Stopped,
    Starting,
    Started,
    Stopping,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Processing {
    Running,
    Pausing,
    Paused,
    Resuming,
}

pub(super) struct PendingUpdate {
    pub request: ApplyUpdateRequest,
    pub attempted: bool,
}

pub(super) struct Lifecycle {
    pub initialization: Initialization,
    pub activation: Activation,
    pub processing: Processing,
    pub stop_hook_complete: bool,
    pub retiring: bool,
    pub disposing: bool,
    pub disposed: bool,
    /// Fixed by the first DISPOSE; retries must repeat it.
    pub dispose: Option<(DisposeReason, StateIntent)>,
    pub request: CreateRequest,
    pub configuration: serde_json::Value,
    pub update: Option<PendingUpdate>,
    pub applied: Option<ApplyUpdateRequest>,
}

fn matches_current_description(current: &CreateRequest, state: &Lifecycle) -> bool {
    let accepted = &state.request;
    // Compare the wire description, not execution-only construction annotations.
    // Neither an old directive nor a credential binding is an update permit.
    current.id == accepted.id
        && current.implementation == accepted.implementation
        && current.configuration_version == accepted.configuration_version
        && current.scope == accepted.scope
        && current.inputs == accepted.inputs
        && (current.configuration == accepted.configuration
            || current.configuration == state.configuration)
}

struct IndependentCalls {
    active: usize,
    generation: u64,
    accepting: bool,
    retired: bool,
}

pub(super) struct StateAdministration {
    lifecycle: Mutex<Lifecycle>,
    independent: Mutex<IndependentCalls>,
    independent_changed: tokio::sync::watch::Sender<()>,
    independent_generation: tokio::sync::watch::Sender<u64>,
}

impl StateAdministration {
    pub(super) fn new(request: CreateRequest, configuration: serde_json::Value) -> Self {
        Self {
            lifecycle: Mutex::new(Lifecycle {
                initialization: Initialization::New,
                activation: Activation::Stopped,
                processing: Processing::Running,
                stop_hook_complete: false,
                retiring: false,
                disposing: false,
                disposed: false,
                dispose: None,
                request,
                configuration,
                update: None,
                applied: None,
            }),
            independent: Mutex::new(IndependentCalls {
                active: 0,
                generation: 0,
                accepting: true,
                retired: false,
            }),
            independent_changed: tokio::sync::watch::channel(()).0,
            independent_generation: tokio::sync::watch::channel(0).0,
        }
    }

    pub(super) fn state(&self) -> Result<MutexGuard<'_, Lifecycle>, Failure> {
        self.lifecycle
            .lock()
            .map_err(|_| Failure::protocol("native lifecycle lock is poisoned"))
    }

    /// True acknowledges an already completed transition without repeating hooks.
    pub(super) fn prepare(
        &self,
        instance: &Instance,
        code: u32,
        context: Option<&LifecycleRequest>,
    ) -> Result<bool, Failure> {
        use abi::operation::*;
        if let Some(context) = context {
            validate_context(instance, code, context)?;
        }
        let mut state = self.state()?;
        if state.retiring && !matches!(code, STOP | DISPOSE) {
            return Err(Failure::closed());
        }
        // Only a started activation owns workers; otherwise there is no
        // failure to observe, which is not itself a failure.
        if code == WORKER_FAILURE {
            return if state.activation == Activation::Started && state.update.is_none() {
                Ok(false)
            } else {
                Err(Failure::closed())
            };
        }
        if state.update.is_some() && !matches!(code, APPLY | STOP | DISPOSE) {
            return Err(Failure::failed("native component has an incomplete update"));
        }
        match code {
            INITIALIZE => match state.initialization {
                Initialization::Complete => return Ok(true),
                Initialization::Incomplete => return Err(Failure::failed(
                    "native initialization did not complete; dispose and construct a new object",
                )),
                Initialization::New => state.initialization = Initialization::Incomplete,
            },
            START => {
                if state.initialization != Initialization::Complete {
                    return Err(Failure::failed(
                        "native component requires successful initialization",
                    ));
                }
                match state.activation {
                    Activation::Started => {
                        if matches!(state.processing, Processing::Pausing | Processing::Resuming) {
                            return Err(Failure::failed(
                                "native processing transition has not completed",
                            ));
                        }
                        return Ok(true);
                    }
                    Activation::Stopped => {
                        state.activation = Activation::Starting;
                        state.stop_hook_complete = false;
                    }
                    Activation::Starting | Activation::Stopping => {
                        return Err(Failure::failed(
                            "native component requires successful cleanup before restart",
                        ))
                    }
                }
            }
            STOP => {
                if state.activation == Activation::Stopped {
                    return Ok(true);
                }
                state.activation = Activation::Stopping;
                instance.running.store(false, Ordering::Release);
                instance.bootstrapping.store(false, Ordering::Release);
            }
            DISPOSE => {
                let request = context
                    .and_then(|context| context.dispose)
                    .expect("validated");
                let intent = (request.reason, request.intent);
                if let Some(fixed) = state.dispose {
                    if fixed != intent {
                        return Err(Failure::protocol(
                            "native DISPOSE retry changed its reason or state intent",
                        ));
                    }
                }
                if state.disposed {
                    return Ok(true);
                }
                if state.activation != Activation::Stopped {
                    return Err(Failure::failed(
                        "native disposal requires successful activation cleanup",
                    ));
                }
                if state.initialization == Initialization::Incomplete
                    && request.intent != StateIntent::Preserve
                {
                    return Err(Failure::failed(
                        "native disposal after incomplete initialization must preserve state",
                    ));
                }
                state.dispose = Some(intent);
                state.retiring = true;
                state.disposing = true;
            }
            PAUSE => {
                if state.activation != Activation::Started
                    || instance.bootstrapping.load(Ordering::Acquire)
                {
                    return Err(Failure::failed(
                        "native pause requires an activated component",
                    ));
                }
                match state.processing {
                    Processing::Paused => return Ok(true),
                    Processing::Running | Processing::Pausing => {
                        state.processing = Processing::Pausing
                    }
                    Processing::Resuming => {
                        return Err(Failure::failed("native resume must complete or be stopped"));
                    }
                }
            }
            RESUME => {
                if state.activation != Activation::Started
                    || instance.bootstrapping.load(Ordering::Acquire)
                {
                    return Err(Failure::failed(
                        "native resume requires an activated component",
                    ));
                }
                match state.processing {
                    Processing::Running => return Ok(true),
                    Processing::Paused | Processing::Resuming => {
                        state.processing = Processing::Resuming
                    }
                    Processing::Pausing => {
                        return Err(Failure::failed("native pause has not completed"));
                    }
                }
            }
            APPLY => {
                let update = context
                    .and_then(|context| context.update.clone())
                    .expect("validated update payload");
                if state.applied.as_ref() == Some(&update) {
                    return Ok(true);
                }
                if state
                    .applied
                    .as_ref()
                    .is_some_and(|applied| applied.operation_id == update.operation_id)
                {
                    return Err(Failure::protocol(
                        "native update operation identity was reused for a different intent",
                    ));
                }
                if state.initialization != Initialization::Complete {
                    return Err(Failure::failed(
                        "native update requires successful initialization",
                    ));
                }
                let boundary = match update.plan.mode {
                    ComponentUpdateMode::InPlace => {
                        state.activation == Activation::Started
                            && state.processing == Processing::Paused
                            && !instance.bootstrapping.load(Ordering::Acquire)
                    }
                    ComponentUpdateMode::Restart => state.activation == Activation::Stopped,
                    ComponentUpdateMode::Recreate => false,
                };
                if !boundary {
                    return Err(Failure::failed(
                        "native update requires its declared pause/stop boundary",
                    ));
                }
                if let Some(pending) = &state.update {
                    if pending.request != update {
                        return Err(Failure::failed(
                            "native component has a different incomplete update",
                        ));
                    }
                } else {
                    if !matches_current_description(&update.request.current, &state) {
                        return Err(Failure::protocol(
                            "native update current request does not match this object's identity or configuration",
                        ));
                    }
                    state.update = Some(PendingUpdate {
                        request: update,
                        attempted: false,
                    });
                }
            }
            CONTROL | WAKE_WAIT | WAKE_PENDING => {
                if state.initialization == Initialization::Incomplete {
                    return Err(Failure::failed("native initialization has not completed"));
                }
            }
            BIND_INPUTS | BOOTSTRAP_STATUS => {
                if state.initialization == Initialization::Incomplete
                    || matches!(
                        state.activation,
                        Activation::Starting | Activation::Stopping
                    )
                {
                    return Err(Failure::failed(
                        "native lifecycle transition has not completed",
                    ));
                }
            }
            BOOTSTRAP_INPUT | BOOTSTRAP_COMPLETE | BOOTSTRAP_RESET | BOOTSTRAP_REBUILD => {
                if state.activation != Activation::Started {
                    return Err(Failure::failed(
                        "native bootstrap requires successful activation",
                    ));
                }
            }
            TRANSACT => {
                if state.initialization == Initialization::Incomplete
                    || state.activation != Activation::Stopped
                    || state.processing != Processing::Running
                    || instance.running.load(Ordering::Acquire)
                    || instance.bootstrapping.load(Ordering::Acquire)
                {
                    return Err(Failure::failed(
                        "native transaction participant has unfinished or active standalone lifecycle",
                    ));
                }
            }
            _ => {
                if state.activation != Activation::Started
                    || state.processing != Processing::Running
                    || !instance.running.load(Ordering::Acquire)
                {
                    return Err(Failure::failed("native component processing is not active"));
                }
            }
        }
        Ok(false)
    }

    pub(super) fn check_processing(&self) -> Result<(), Failure> {
        let state = self.state()?;
        if state.retiring
            || state.update.is_some()
            || state.activation != Activation::Started
            || state.processing != Processing::Running
        {
            return Err(Failure::closed());
        }
        Ok(())
    }

    pub(super) fn fence_independent(&self, permanent: bool) -> Result<(), Failure> {
        let generation = {
            let mut calls = self
                .independent
                .lock()
                .map_err(|_| Failure::protocol("native independent-call lock is poisoned"))?;
            calls.accepting = false;
            calls.retired |= permanent;
            calls.generation = calls
                .generation
                .checked_add(1)
                .ok_or_else(|| Failure::protocol("native independent-call generation exhausted"))?;
            calls.generation
        };
        self.independent_generation.send_replace(generation);
        Ok(())
    }

    pub(super) fn activate_independent(&self) -> Result<(), Failure> {
        let mut calls = self
            .independent
            .lock()
            .map_err(|_| Failure::protocol("native independent-call lock is poisoned"))?;
        if calls.retired {
            return Err(Failure::closed());
        }
        calls.accepting = true;
        Ok(())
    }

    pub(super) fn claim_independent(
        self: &Arc<Self>,
        damaged: Arc<transport::PanicState>,
    ) -> Result<IndependentGuard, Failure> {
        let mut calls = self
            .independent
            .lock()
            .map_err(|_| Failure::protocol("native independent-call lock is poisoned"))?;
        if !calls.accepting || calls.retired {
            return Err(Failure::closed());
        }
        calls.active = calls
            .active
            .checked_add(1)
            .ok_or_else(|| Failure::protocol("too many independent native operations"))?;
        Ok(IndependentGuard {
            administration: self.clone(),
            generation: calls.generation,
            changed: self.independent_generation.subscribe(),
            damaged,
        })
    }

    pub(super) async fn wait_for_independent_calls(&self) -> anyhow::Result<()> {
        let mut changed = self.independent_changed.subscribe();
        loop {
            let active = self
                .independent
                .lock()
                .map_err(|_| anyhow::anyhow!("native independent-call lock is poisoned"))?
                .active;
            if active == 0 {
                return Ok(());
            }
            changed.changed().await?;
        }
    }
}

pub(super) struct IndependentGuard {
    administration: Arc<StateAdministration>,
    generation: u64,
    changed: tokio::sync::watch::Receiver<u64>,
    damaged: Arc<transport::PanicState>,
}

impl IndependentGuard {
    pub(super) async fn run(
        mut self,
        work: impl Future<Output = Result<Vec<u8>, Failure>> + Send,
    ) -> Result<Vec<u8>, Failure> {
        let generation = self.generation;
        let result = tokio::select! {
            biased;
            _ = self.changed.wait_for(|current| *current != generation) => Err(Failure::closed()),
            result = work => result,
        };
        if *self.changed.borrow() != generation {
            return Err(Failure::closed());
        }
        result
    }
}

impl Drop for IndependentGuard {
    fn drop(&mut self) {
        {
            let mut calls = self
                .administration
                .independent
                .lock()
                .unwrap_or_else(|error| {
                    self.damaged.quarantine();
                    error.into_inner()
                });
            calls.active -= 1;
        }
        self.administration.independent_changed.send_modify(|_| ());
    }
}

/// Rejects malformed contexts before any plugin code runs.
fn validate_context(
    instance: &Instance,
    code: u32,
    context: &LifecycleRequest,
) -> Result<(), Failure> {
    use abi::operation::*;
    if context.version != LifecycleRequest::VERSION {
        return Err(Failure::protocol(
            "unsupported native lifecycle context version",
        ));
    }
    if let Some(required) = context.required.first() {
        return Err(Failure::protocol(format!(
            "native lifecycle context requires unsupported semantics `{required}`"
        )));
    }
    if context.component_id != instance.metadata.descriptor.id().as_str() {
        return Err(Failure::protocol(
            "native lifecycle context names a different component",
        ));
    }
    drasi_computation_contracts::validate_identifier(
        "native lifecycle operation",
        &context.operation_id,
    )
    .map_err(|error| Failure::protocol(format!("{error:#}")))?;
    // Bound factories receive credentials on START and on an erasing
    // DISPOSE, which may have to reach the external system without a start.
    let credentials = instance.activation_credentials_bound
        && (code == START
            || (code == DISPOSE
                && context
                    .dispose
                    .is_some_and(|dispose| dispose.intent == StateIntent::Erase)));
    if context.credentials.is_some() != credentials
        || context.stop_mode.is_some() != (code == STOP)
        || context.dispose.is_some() != (code == DISPOSE)
        || context.update.is_some() != (code == APPLY)
    {
        return Err(Failure::protocol(
            "native lifecycle context fields do not match the operation",
        ));
    }
    if let Some(credentials) = &context.credentials {
        credentials
            .validate()
            .map_err(|_| Failure::protocol("invalid activation credential payload"))?;
    }
    if let Some(dispose) = context.dispose {
        dispose
            .reason
            .validate(dispose.intent)
            .map_err(|error| Failure::protocol(error.to_string()))?;
        if dispose.intent == StateIntent::Erase && !instance.factory.capabilities.erases_owned_state
        {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "native component does not erase owned state",
            ));
        }
        if dispose.attempt == 0 {
            return Err(Failure::protocol("native dispose attempts start at one"));
        }
    }
    Ok(())
}
