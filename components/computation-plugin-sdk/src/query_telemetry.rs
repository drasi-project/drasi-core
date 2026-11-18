// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host-owned, allowlisted query observations. No metric instruments are created here.

use crate::telemetry::*;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, TryLockError},
    task::{Context, Poll},
};

#[derive(Clone)]
pub struct QueryTelemetry {
    binding: Option<PluginTelemetry>,
}

impl QueryTelemetry {
    pub fn capture() -> Result<Self> {
        Ok(Self {
            binding: ExecutionScope::current_plugin_telemetry(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.binding
            .as_ref()
            .is_some_and(PluginTelemetry::supports_query_observations)
    }

    pub fn available(&self) -> bool {
        self.enabled()
    }

    pub fn begin(
        &self,
        kind: QueryObservationKind,
        phase: QueryObservationPhase,
        change_count: Option<u64>,
    ) -> Result<Option<QueryObservationGuard>> {
        let Some(binding) = self.binding.as_ref().filter(|_| self.enabled()) else {
            return Ok(None);
        };
        let request = QueryObservationRequest {
            kind,
            phase,
            parent: ExecutionContext::current().trace().cloned(),
            change_count,
        };
        request.validate()?;
        let Some(observation) = binding.begin_query_observation(&request)? else {
            return Ok(None);
        };
        let scope = ExecutionScope::capture()
            .with_component(binding.config().scope.clone(), binding.config().generation)
            .with_plugin_telemetry(Some(binding.clone()));
        let context = scope
            .context()
            .clone()
            .with_trace_from(&observation.context());
        Ok(Some(QueryObservationGuard {
            scope: QueryScope(Arc::new(ScopeState {
                observation: Mutex::new(Some(observation)),
                execution: scope.with_context(context),
            })),
        }))
    }

    pub fn event(&self, event: QueryObservationEvent) -> Result<()> {
        event.validate()?;
        if !self.enabled() {
            return Ok(());
        }
        self.binding
            .as_ref()
            .map_or(Ok(()), |binding| binding.query_observation_event(event))
    }
}

struct ScopeState {
    observation: Mutex<Option<Box<dyn QueryObservation>>>,
    execution: ExecutionScope,
}

#[derive(Clone)]
pub(crate) struct QueryScope(Arc<ScopeState>);

thread_local! {
    static CURRENT: RefCell<Option<QueryScope>> = const { RefCell::new(None) };
}

impl QueryScope {
    pub(crate) fn current() -> Option<Self> {
        CURRENT.with(|current| current.borrow().clone())
    }

    pub(crate) fn in_scope<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        // A processing callback may synchronously re-enter its initiating query.
        // Do not recursively lock the same local observation in that case.
        if CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .is_some_and(|scope| Arc::ptr_eq(&scope.0, &self.0))
        }) {
            return Ok(self.0.execution.in_scope(work));
        }
        let observation = self
            .0
            .observation
            .try_lock()
            .map_err(observation_lock_error)?;
        let observation = observation.as_ref().ok_or(TelemetryError::Retired)?;
        let mut work = Some(work);
        let mut result = None;
        let mut repeated = false;
        observation.in_scope(&mut || {
            if let Some(work) = work.take() {
                struct Restore(Option<QueryScope>);
                impl Drop for Restore {
                    fn drop(&mut self) {
                        CURRENT.with(|current| current.replace(self.0.take()));
                    }
                }
                let _restore = Restore(CURRENT.with(|current| current.replace(Some(self.clone()))));
                result = Some(self.0.execution.in_scope(work));
            } else {
                repeated = true;
            }
        })?;
        if repeated {
            return Err(TelemetryError::Invalid(
                "query observation scope invoked its callback twice".into(),
            ));
        }
        result.ok_or_else(|| {
            TelemetryError::Invalid("query observation scope did not invoke its callback".into())
        })
    }
}

pub struct QueryObservationGuard {
    scope: QueryScope,
}

impl QueryObservationGuard {
    pub fn context(&self) -> ExecutionContext {
        self.scope.0.execution.context().clone()
    }

    pub fn in_scope(&self, work: &mut dyn FnMut()) -> Result<()> {
        self.scope.in_scope(work)
    }

    pub fn scope<F, T, E>(&self, future: F) -> QueryScopedFuture<F>
    where
        F: Future<Output = std::result::Result<T, E>>,
        E: From<TelemetryError>,
    {
        QueryScopedFuture {
            scope: self.scope.clone(),
            future: Some(Box::pin(future)),
        }
    }

    pub fn finish(
        &mut self,
        outcome: QueryObservationOutcome,
        result_count: Option<u64>,
    ) -> Result<()> {
        let result = QueryObservationResult {
            outcome,
            result_count,
        };
        result.validate()?;
        if let Some(mut observation) = self
            .scope
            .0
            .observation
            .try_lock()
            .map_err(observation_lock_error)?
            .take()
        {
            observation.finish(result)?;
        }
        Ok(())
    }
}

fn observation_lock_error<T>(error: TryLockError<T>) -> TelemetryError {
    match error {
        TryLockError::WouldBlock => {
            TelemetryError::Invalid("native query observation is busy".into())
        }
        TryLockError::Poisoned(_) => TelemetryError::Poisoned,
    }
}

impl Drop for QueryObservationGuard {
    fn drop(&mut self) {
        if let Err(error) = self.finish(QueryObservationOutcome::Cancelled, None) {
            log::error!("Cancelling native query observation: {error}");
        }
    }
}

pub struct QueryScopedFuture<F> {
    scope: QueryScope,
    future: Option<Pin<Box<F>>>,
}

impl<F, T, E> Future for QueryScopedFuture<F>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: From<TelemetryError>,
{
    type Output = std::result::Result<T, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let scope = this.scope.clone();
        match scope.in_scope(|| {
            this.future
                .as_mut()
                .expect("polled after completion")
                .as_mut()
                .poll(cx)
        }) {
            Ok(poll) => poll,
            Err(error) => Poll::Ready(Err(error.into())),
        }
    }
}

impl<F> Drop for QueryScopedFuture<F> {
    fn drop(&mut self) {
        let scope = self.scope.clone();
        if let Err(error) = scope.in_scope(|| drop(self.future.take())) {
            log::error!("Entering native query scope for future cancellation: {error}");
        }
    }
}

/// Processing work is polled by a different runtime after the borrowing call.
#[derive(Clone)]
pub(crate) struct WorkScope {
    execution: ExecutionScope,
    query: Option<QueryScope>,
}

impl WorkScope {
    pub(crate) fn capture() -> Self {
        Self {
            execution: ExecutionScope::capture(),
            query: QueryScope::current(),
        }
    }

    pub(crate) fn in_scope<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        match &self.query {
            Some(query) => query.in_scope(|| self.execution.in_scope(work)),
            None => Ok(self.execution.in_scope(work)),
        }
    }

    pub(crate) fn scope<F>(&self, future: F) -> WorkFuture<F> {
        WorkFuture {
            scope: self.clone(),
            future: Some(Box::pin(future)),
        }
    }
}

pub(crate) struct WorkFuture<F> {
    scope: WorkScope,
    future: Option<Pin<Box<F>>>,
}

impl<F, T, E> Future for WorkFuture<F>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: From<TelemetryError>,
{
    type Output = std::result::Result<T, E>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.scope.in_scope(|| {
            this.future
                .as_mut()
                .expect("polled after completion")
                .as_mut()
                .poll(cx)
        }) {
            Ok(poll) => poll,
            Err(error) => Poll::Ready(Err(error.into())),
        }
    }
}

impl<F> Drop for WorkFuture<F> {
    fn drop(&mut self) {
        if let Err(error) = self.scope.in_scope(|| drop(self.future.take())) {
            log::error!("Entering native processing work cancellation scope: {error}");
        }
    }
}
