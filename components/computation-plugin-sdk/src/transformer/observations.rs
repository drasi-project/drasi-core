// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Transformer processing observations. Factories that do not declare query
//! instrumentation receive a disabled binding, so every call is a no-op.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use crate::query_telemetry::{QueryObservationGuard, QueryTelemetry};
use anyhow::Result;
use drasi_computation_contracts::{
    telemetry::{
        ExecutionScope, QueryObservationEvent, QueryObservationKind, QueryObservationOutcome,
        QueryObservationPhase,
    },
    ChangeEnvelope, ChangeOperation,
};

#[derive(Clone)]
pub struct Observations(QueryTelemetry);

impl Observations {
    /// The plugin telemetry binding, for logic that observes its own evaluation.
    pub fn telemetry(&self) -> &QueryTelemetry {
        &self.0
    }

    pub fn capture() -> Result<Self> {
        Ok(Self(QueryTelemetry::capture()?))
    }

    pub fn enabled(&self) -> bool {
        self.0.enabled()
    }

    pub async fn phase<T>(
        &self,
        kind: QueryObservationKind,
        phase: QueryObservationPhase,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let Some(mut observation) = self.0.begin(kind, phase, None)? else {
            return future.await;
        };
        let result = observation.scope(future).await;
        let finished = observation.finish(
            if result.is_ok() {
                QueryObservationOutcome::Success
            } else {
                QueryObservationOutcome::Error
            },
            None,
        );
        match (result, finished) {
            (Err(error), Err(telemetry)) => Err(error.context(telemetry)),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    pub async fn transaction<T>(
        &self,
        phase: QueryObservationPhase,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        self.phase(QueryObservationKind::Transaction, phase, future)
            .await
    }

    pub fn input(&self, phase: QueryObservationPhase) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        Ok(self.0.event(QueryObservationEvent::Input { phase })?)
    }

    pub fn results(&self, output: &ChangeEnvelope) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        let mut counts = [0; 3];
        for operation in output.changes().operations() {
            counts[match operation {
                ChangeOperation::Added { .. } => 0,
                ChangeOperation::Updated { .. } => 1,
                ChangeOperation::Deleted { .. } => 2,
            }] += 1;
        }
        Ok(self.0.event(QueryObservationEvent::Results {
            added: counts[0],
            updated: counts[1],
            deleted: counts[2],
        })?)
    }

    pub fn state(&self, outbox: usize, live: usize) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        Ok(self.0.event(QueryObservationEvent::State {
            outbox: u64::try_from(outbox)?,
            live: u64::try_from(live)?,
        })?)
    }

    pub fn run(&self, phase: QueryObservationPhase) -> Run {
        Run {
            telemetry: self.0.clone(),
            phase,
            initiator: self.enabled().then(ExecutionScope::capture),
            observation: self.enabled().then(|| Arc::new(Mutex::new(None))),
        }
    }
}

/// Selected only after actual work is known, but retained through host commit.
#[derive(Clone)]
pub struct Run {
    telemetry: QueryTelemetry,
    phase: QueryObservationPhase,
    initiator: Option<ExecutionScope>,
    observation: Option<Arc<Mutex<Option<Arc<QueryObservationGuard>>>>>,
}

impl Run {
    pub fn start(&self, kind: QueryObservationKind) -> Result<()> {
        let Some(observation) = &self.observation else {
            return Ok(());
        };
        let mut current = observation
            .lock()
            .map_err(|_| anyhow::anyhow!("query observation ownership poisoned"))?;
        if current.is_none() {
            *current = self
                .initiator
                .as_ref()
                .expect("enabled query scope")
                .in_scope(|| self.telemetry.begin(kind, self.phase, None))?
                .map(Arc::new);
        }
        Ok(())
    }

    pub async fn scope<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        if self.observation.is_none() {
            return future.await;
        }
        RunFuture {
            run: self,
            future: Some(Box::pin(future)),
        }
        .await
    }
}

struct RunFuture<'a, F> {
    run: &'a Run,
    future: Option<Pin<Box<F>>>,
}

impl<F, T> Future for RunFuture<'_, F>
where
    F: Future<Output = Result<T>>,
{
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let observation = match this
            .run
            .observation
            .as_ref()
            .expect("enabled query run")
            .lock()
        {
            Ok(observation) => observation.clone(),
            Err(_) => {
                return Poll::Ready(Err(anyhow::anyhow!("query observation ownership poisoned")))
            }
        };
        let future = this
            .future
            .as_mut()
            .expect("query work is retained until drop");
        match observation {
            Some(observation) => {
                let mut result = None;
                match observation.in_scope(&mut || result = Some(future.as_mut().poll(cx))) {
                    Ok(()) => result.unwrap_or_else(|| {
                        Poll::Ready(Err(anyhow::anyhow!("query scope did not poll its work")))
                    }),
                    Err(error) => Poll::Ready(Err(error.into())),
                }
            }
            None => future.as_mut().poll(cx),
        }
    }
}

impl<F> Drop for RunFuture<'_, F> {
    fn drop(&mut self) {
        let mut future = self.future.take();
        let observation = self
            .run
            .observation
            .as_ref()
            .expect("enabled query run")
            .lock()
            .map(|observation| observation.clone());
        match observation {
            Ok(Some(observation)) => {
                if let Err(error) = observation.in_scope(&mut || drop(future.take())) {
                    log::error!("Cannot restore native query context during cleanup: {error}");
                }
            }
            Ok(None) => {}
            Err(error) => log::error!("Cannot read native query cleanup context: {error}"),
        }
        drop(future);
    }
}

impl Run {
    pub fn finish(&self, outcome: QueryObservationOutcome, outputs: Option<u64>) -> Result<()> {
        let Some(observations) = &self.observation else {
            return Ok(());
        };
        let observation = observations
            .lock()
            .map_err(|_| anyhow::anyhow!("query observation ownership poisoned"))?
            .take();
        if let Some(observation) = observation {
            let mut observation = Arc::try_unwrap(observation)
                .map_err(|_| anyhow::anyhow!("query observation still has active scoped work"))?;
            observation.finish(outcome, outputs)?;
        }
        Ok(())
    }

    pub fn context(
        &self,
    ) -> Result<Option<drasi_computation_contracts::telemetry::ExecutionContext>> {
        let Some(observations) = &self.observation else {
            return Ok(None);
        };
        Ok(observations
            .lock()
            .map_err(|_| anyhow::anyhow!("query observation ownership poisoned"))?
            .as_ref()
            .map(|observation| observation.context()))
    }
}
