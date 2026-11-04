// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    ComponentObservations, ExecutionContext, ExecutionScope, ObservedOperation,
    OperationObservation, TelemetryError,
};
use std::{
    cell::RefCell,
    future::Future,
    sync::{Arc, Mutex},
};
use tracing::Instrument;

struct State {
    observation: Option<OperationObservation>,
    closed: bool,
}

/// A query-local, lazily started bootstrap run. Never part of persisted state.
#[derive(Clone)]
pub struct BootstrapTrace {
    owner: Arc<ComponentObservations>,
    scope: ExecutionScope,
    start: ExecutionContext,
    state: Arc<Mutex<State>>,
}

thread_local! {
    static CURRENT: RefCell<Option<BootstrapTrace>> = const { RefCell::new(None) };
}

struct CurrentGuard(Option<BootstrapTrace>);

impl Drop for CurrentGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.0.take()));
    }
}

impl BootstrapTrace {
    pub(crate) fn capture() -> Self {
        Self {
            owner: ComponentObservations::current_or_disabled(),
            scope: ExecutionScope::capture(),
            start: ExecutionContext::current(),
            state: Arc::new(Mutex::new(State {
                observation: None,
                closed: false,
            })),
        }
    }

    pub fn current() -> Option<Self> {
        CURRENT.with(|current| current.borrow().clone())
    }

    /// Call immediately before confirmed snapshot initialization, not replay.
    pub fn start(&self) -> Result<ExecutionContext, TelemetryError> {
        let mut state = self.state.lock().map_err(|_| TelemetryError::Poisoned)?;
        if state.closed {
            return Err(TelemetryError::Invalid(
                "bootstrap trace is already closed".into(),
            ));
        }
        if state.observation.is_none() {
            state.observation = Some(self.scope.in_scope(|| {
                self.owner.root(
                    ObservedOperation::Bootstrap,
                    &self.start,
                    Default::default(),
                )
            })?);
        }
        Ok(state
            .observation
            .as_ref()
            .expect("started bootstrap")
            .context()
            .clone())
    }

    /// Propagates lazy initialization ownership while preserving poll/drop scope.
    pub async fn scope<T>(&self, future: impl Future<Output = T>) -> Result<T, TelemetryError> {
        let (context, span) = {
            let state = self.state.lock().map_err(|_| TelemetryError::Poisoned)?;
            match &state.observation {
                Some(observation) => (observation.context().clone(), observation.span()),
                None => (self.start.clone(), tracing::Span::none()),
            }
        };
        let future = ExecutionScope::capture()
            .with_context(context)
            .scope(future.instrument(span));
        let mut future = std::pin::pin!(future);
        Ok(self
            .owner
            .scope(std::future::poll_fn(|cx| {
                let _current =
                    CurrentGuard(CURRENT.with(|current| current.replace(Some(self.clone()))));
                future.as_mut().poll(cx)
            }))
            .await)
    }

    pub(crate) fn guard(&self) -> BootstrapTraceGuard {
        BootstrapTraceGuard(self.clone())
    }

    fn finish(&self, outcome: usize) {
        let mut state = self.state.lock().unwrap_or_else(|error| {
            log::error!("Closing poisoned bootstrap telemetry: {error}");
            error.into_inner()
        });
        if !state.closed {
            if let Some(mut observation) = state.observation.take() {
                observation.finish(outcome);
            }
            state.closed = true;
        }
    }
}

pub(crate) struct BootstrapTraceGuard(BootstrapTrace);

impl BootstrapTraceGuard {
    pub(crate) fn finish(&self, success: bool) {
        self.0.finish(if success { 0 } else { 1 });
    }
}

impl Drop for BootstrapTraceGuard {
    fn drop(&mut self) {
        self.0.finish(2);
    }
}
