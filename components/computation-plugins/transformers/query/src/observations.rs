// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Observation of the query engine's own evaluation. Everything else a query
//! observes is shared transformer telemetry.

use std::sync::Arc;

use drasi_computation_contracts::telemetry::{
    QueryObservationKind, QueryObservationOutcome, QueryObservationPhase,
};
use drasi_computation_plugin_sdk::{
    query_telemetry::{QueryObservationGuard, QueryTelemetry},
    transformer::Observations,
};
use drasi_core::{
    computation::{QueryInputBatch, QueryObservation, QueryObserver, QueryOperation},
    evaluation::context::QueryPartEvaluationContext,
    interface::IndexError,
    query::QueryEvaluator,
};

fn observer(
    observations: &Observations,
    phase: QueryObservationPhase,
) -> Option<Arc<dyn QueryObserver>> {
    observations.enabled().then(|| {
        Arc::new(EvaluatorObserver {
            telemetry: observations.telemetry().clone(),
            phase,
        }) as Arc<dyn QueryObserver>
    })
}

pub(super) async fn evaluate<const SOURCE_TRANSACTION: bool>(
    observations: &Observations,
    evaluator: &QueryEvaluator,
    input: QueryInputBatch,
    phase: QueryObservationPhase,
) -> drasi_core::computation::Result<Vec<QueryPartEvaluationContext>> {
    let observer = observer(observations, phase);
    evaluator
        .evaluate_input_batch_observed::<SOURCE_TRANSACTION>(input, observer.as_ref())
        .await
}

pub(super) async fn future(
    observations: &Observations,
    evaluator: &QueryEvaluator,
) -> drasi_core::computation::Result<Option<Arc<drasi_core::computation::ComputationFutureResult>>>
{
    let observer = observer(observations, QueryObservationPhase::Steady);
    evaluator
        .evaluate_next_future_observed(observer.as_ref())
        .await
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
