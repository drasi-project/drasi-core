// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use anyhow::Result;
use drasi_computation_contracts::{
    telemetry::{
        ExecutionScope, QueryObservationEvent, QueryObservationKind, QueryObservationOutcome,
        QueryObservationPhase,
    },
    ChangeEnvelope, ChangeOperation,
};
use drasi_computation_plugin_sdk::query_telemetry::{QueryObservationGuard, QueryTelemetry};
use drasi_core::{
    computation::{QueryInputBatch, QueryObservation, QueryObserver, QueryOperation},
    evaluation::context::QueryPartEvaluationContext,
    interface::IndexError,
    query::QueryEvaluator,
};

#[derive(Clone)]
pub(super) struct QueryObservations(QueryTelemetry);

impl QueryObservations {
    pub(super) fn capture() -> Result<Self> {
        Ok(Self(QueryTelemetry::capture()?))
    }

    pub(super) fn enabled(&self) -> bool {
        self.0.enabled()
    }

    pub(super) async fn phase<T>(
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

    pub(super) async fn transaction<T>(
        &self,
        phase: QueryObservationPhase,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        self.phase(QueryObservationKind::Transaction, phase, future)
            .await
    }

    fn observer(&self, phase: QueryObservationPhase) -> Option<Arc<dyn QueryObserver>> {
        self.0.enabled().then(|| {
            Arc::new(EvaluatorObserver {
                telemetry: self.0.clone(),
                phase,
            }) as Arc<dyn QueryObserver>
        })
    }

    pub(super) async fn evaluate<const SOURCE_TRANSACTION: bool>(
        &self,
        evaluator: &QueryEvaluator,
        input: QueryInputBatch,
        phase: QueryObservationPhase,
    ) -> drasi_core::computation::Result<Vec<QueryPartEvaluationContext>> {
        let observer = self.observer(phase);
        evaluator
            .evaluate_input_batch_observed::<SOURCE_TRANSACTION>(input, observer.as_ref())
            .await
    }

    pub(super) async fn future(
        &self,
        evaluator: &QueryEvaluator,
    ) -> drasi_core::computation::Result<
        Option<Arc<drasi_core::computation::ComputationFutureResult>>,
    > {
        let observer = self.observer(QueryObservationPhase::Steady);
        evaluator
            .evaluate_next_future_observed(observer.as_ref())
            .await
    }

    pub(super) fn input(&self, phase: QueryObservationPhase) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        Ok(self.0.event(QueryObservationEvent::Input { phase })?)
    }

    pub(super) fn results(&self, output: &ChangeEnvelope) -> Result<()> {
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

    pub(super) fn state(&self, outbox: usize, live: usize) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        Ok(self.0.event(QueryObservationEvent::State {
            outbox: u64::try_from(outbox)?,
            live: u64::try_from(live)?,
        })?)
    }

    pub(super) fn run(&self, phase: QueryObservationPhase) -> QueryRun {
        QueryRun {
            telemetry: self.0.clone(),
            phase,
            initiator: self.enabled().then(ExecutionScope::capture),
            observation: self.enabled().then(|| Arc::new(Mutex::new(None))),
        }
    }
}

struct EvaluatorObserver {
    telemetry: QueryTelemetry,
    phase: QueryObservationPhase,
}

impl QueryObserver for EvaluatorObserver {
    fn start(
        &self,
        operation: QueryOperation,
        change_count: Option<usize>,
    ) -> Result<Box<dyn QueryObservation>, IndexError> {
        if operation != QueryOperation::Evaluate {
            return Err(IndexError::other(std::io::Error::other(
                "native query commit observation belongs to the host transaction owner",
            )));
        }
        let observation = self
            .telemetry
            .begin(
                QueryObservationKind::Evaluate,
                self.phase,
                change_count
                    .map(u64::try_from)
                    .transpose()
                    .map_err(IndexError::other)?,
            )
            .map_err(IndexError::other)?;
        Ok(Box::new(EvaluationObservation(observation)))
    }
}

struct EvaluationObservation(Option<QueryObservationGuard>);

impl QueryObservation for EvaluationObservation {
    fn in_scope(&self, work: &mut dyn FnMut()) {
        if let Err(error) = self.in_scope_result(work) {
            log::error!("Cannot enter native query evaluation observation: {error}");
        }
    }

    fn in_scope_result(&self, work: &mut dyn FnMut()) -> Result<(), IndexError> {
        let Some(observation) = &self.0 else {
            work();
            return Ok(());
        };
        observation.in_scope(work).map_err(IndexError::other)
    }

    fn finish(&mut self, success: bool, result_count: Option<usize>) {
        if let Some(mut observation) = self.0.take() {
            let result = observation.finish(
                if success {
                    QueryObservationOutcome::Success
                } else {
                    QueryObservationOutcome::Error
                },
                result_count.map(|count| count as u64),
            );
            if let Err(error) = result {
                log::error!("Cannot finish native query evaluation observation: {error}");
            }
        }
    }
}

/// Selected only after actual work is known, but retained through host commit.
#[derive(Clone)]
pub(super) struct QueryRun {
    telemetry: QueryTelemetry,
    phase: QueryObservationPhase,
    initiator: Option<ExecutionScope>,
    observation: Option<Arc<Mutex<Option<Arc<QueryObservationGuard>>>>>,
}

impl QueryRun {
    pub(super) fn start(&self, kind: QueryObservationKind) -> Result<()> {
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

    pub(super) async fn scope<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        if self.observation.is_none() {
            return future.await;
        }
        QueryRunFuture {
            run: self,
            future: Some(Box::pin(future)),
        }
        .await
    }
}

struct QueryRunFuture<'a, F> {
    run: &'a QueryRun,
    future: Option<Pin<Box<F>>>,
}

impl<F, T> Future for QueryRunFuture<'_, F>
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

impl<F> Drop for QueryRunFuture<'_, F> {
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

impl QueryRun {
    pub(super) fn finish(
        &self,
        outcome: QueryObservationOutcome,
        outputs: Option<u64>,
    ) -> Result<()> {
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

    pub(super) fn context(
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
