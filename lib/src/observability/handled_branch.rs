// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    ComponentObservations, ExecutionContext, GraphTelemetry, HandledBranchContext,
    HandledBranchKey, HandledBranchOutput, HandledBranchTracker, HandledBranchUnavailable,
    TelemetryError,
};
use drasi_computation_contracts::{
    ChangeEnvelope, GraphProducerProgress, QueryChangeCodec, QueryRecoveryIdentity,
};
use metrics::Histogram;
use std::sync::Arc;

pub(crate) const HANDLED_LATENCY: &str = "drasi.pipeline.end_to_end.duration";

/// A positively observable sink branch. Keeps the original origin live through
/// handling but never retains the component owner/cache.
#[derive(Debug, Clone)]
pub(crate) struct HandledBranchObservation {
    tracker: Arc<HandledBranchTracker>,
    context: ExecutionContext,
    key: HandledBranchKey,
    histogram: Histogram,
    lifetime: super::TelemetryLifetime,
}

impl HandledBranchObservation {
    pub(crate) fn finish(&self, rows: Option<&[usize]>) {
        let Some(token) = self
            .context
            .handled_branch()
            .and_then(HandledBranchContext::token)
        else {
            return;
        };
        let result = self.lifetime.with_active(|| {
            let elapsed = match rows {
                Some(rows) => self.tracker.handled_rows(token, &self.key, rows)?,
                None => self.tracker.handled_all(token, &self.key)?,
            };
            if let Some(elapsed) = elapsed {
                self.histogram.record(elapsed.as_secs_f64());
            }
            Ok(())
        });
        if let Err(error) = result {
            timing_error(&self.tracker, "completion", &error);
        }
    }
}

pub(crate) fn timing_error(
    tracker: &HandledBranchTracker,
    operation: &str,
    error: &TelemetryError,
) {
    // Capacity/retirement may affect every input. Keep reports bounded without
    // requiring a recorder, tracing layer, or another diagnostic queue.
    let count = tracker.diagnostics().map(|state| {
        state
            .capacity_rejections
            .saturating_add(state.invalid_requests)
            .saturating_add(state.retired_requests)
            .saturating_add(state.sealed_requests)
    });
    if count
        .as_ref()
        .map_or(true, |count| *count <= 1 || count.is_power_of_two())
    {
        log::warn!("Handled-branch {operation} observation unavailable: {error}");
    }
}

impl GraphTelemetry {
    pub(crate) fn source_timing(
        &self,
        context: &ExecutionContext,
        envelope: &ChangeEnvelope,
    ) -> ExecutionContext {
        let Some(tracker) = self.handled_branch_tracker() else {
            return context.clone();
        };
        if context.handled_branch().is_some() {
            return context.clone();
        }
        if envelope.changes().operations().is_empty() {
            return context
                .clone()
                .with_handled_branch(HandledBranchContext::Unavailable(
                    HandledBranchUnavailable::Unsupported,
                ));
        }
        let timing = match tracker.start() {
            Ok(context) => context,
            Err(error) => {
                timing_error(tracker, "ingress", &error);
                HandledBranchContext::Unavailable(HandledBranchUnavailable::Capacity)
            }
        };
        context.clone().with_handled_branch(timing)
    }

    pub(crate) fn output_timing(
        &self,
        context: &ExecutionContext,
        envelope: &ChangeEnvelope,
    ) -> ExecutionContext {
        if self.handled_branch_tracker().is_none() || context.handled_branch().is_none() {
            return context.clone();
        }
        if !QueryChangeCodec::is_query_output(envelope) {
            return context.clone();
        }
        if QueryChangeCodec::is_snapshot(envelope)
            || QueryChangeCodec::is_progress_only(envelope)
            || envelope.changes().operations().is_empty()
        {
            return context
                .clone()
                .with_handled_branch(HandledBranchContext::Unavailable(
                    HandledBranchUnavailable::Bootstrap,
                ));
        }
        let attribution = (|| -> anyhow::Result<_> {
            let identity = QueryRecoveryIdentity::from_envelope(envelope)?;
            let sequence = QueryChangeCodec::query_sequence(envelope)?;
            let generation = QueryChangeCodec::query_generation(envelope)?;
            let producer = match GraphProducerProgress::from_query_envelope(envelope)? {
                Some(progress) => progress.identity().clone(),
                None => identity.output_producer(envelope.system().stream().clone(), generation)?,
            };
            anyhow::ensure!(
                producer.component_id().as_str() == identity.query_id(),
                "query output timing producer does not match its query"
            );
            let output = HandledBranchOutput {
                query_id: identity.query_id().into(),
                producer_epoch: *producer.incarnation().as_bytes(),
                sequence,
                required_rows: envelope.changes().operations().len(),
            };
            if let Some(inherited) = context.handled_output() {
                if self.scope().and_then(super::TelemetryScope::semantic_kind)
                    != Some(drasi_computation_contracts::ComponentSemanticKind::Query)
                {
                    anyhow::ensure!(
                        inherited == &output,
                        "timing output attribution changed across a non-query transformer"
                    );
                }
            }
            Ok(output)
        })();
        match attribution {
            Ok(output) => context.clone().with_handled_output(output),
            Err(error) => {
                log::warn!("Cannot attribute handled-branch query output: {error:#}");
                context
                    .clone()
                    .with_handled_branch(HandledBranchContext::Unavailable(
                        HandledBranchUnavailable::Unsupported,
                    ))
            }
        }
    }
}

impl ComponentObservations {
    pub(crate) fn handled_branch(
        &self,
        context: &ExecutionContext,
    ) -> Option<HandledBranchObservation> {
        let tracker = self.config.handled_branch_tracker()?;
        let token = match context.handled_branch() {
            Some(HandledBranchContext::Tracked(lease)) => lease.token(),
            reason => {
                let reason = match reason {
                    Some(HandledBranchContext::Unavailable(reason)) => Some(*reason),
                    _ => None,
                };
                if let Err(error) = tracker.unavailable(reason) {
                    timing_error(tracker, "coverage", &error);
                }
                return None;
            }
        };
        let output = context.handled_output()?;
        let sink = self.config.scope()?.component_id()?;
        let key = HandledBranchKey {
            producer_epoch: output.producer_epoch,
            output_sequence: output.sequence,
            sink: sink.as_str().into(),
        };
        let result = self.lifetime.with_active(|| {
            let mut handles = self
                .handled_histograms
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?;
            let histogram = match handles.get(output.query_id.as_ref()) {
                Some(histogram) => histogram.clone(),
                None => {
                    let histogram = self
                        .config
                        .register(
                            HANDLED_LATENCY,
                            vec![
                                ("query_id".into(), output.query_id.to_string()),
                                ("phase".into(), "steady".into()),
                            ],
                        )?
                        .histogram()?;
                    handles.insert(output.query_id.clone(), histogram.clone());
                    histogram
                }
            };
            tracker.prepare(token, &key, output.required_rows)?;
            Ok(histogram)
        });
        match result {
            Ok(histogram) => Some(HandledBranchObservation {
                tracker: tracker.clone(),
                context: context.clone(),
                key,
                histogram,
                lifetime: self.lifetime.clone(),
            }),
            Err(error) => {
                timing_error(tracker, "admission", &error);
                None
            }
        }
    }
}
