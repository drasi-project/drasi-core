// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    ComponentObservations, ExecutionContext, ExecutionScope, ObservedOperation, OperationFields,
    OperationObservation, TelemetryError,
};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::time::Instant;

/// A host-local component binding, not a message's parent or a lifecycle span.
#[derive(Clone, Debug)]
pub struct WorkerTelemetry {
    owner: Arc<ComponentObservations>,
    scope: ExecutionScope,
}

impl Default for WorkerTelemetry {
    fn default() -> Self {
        Self::capture()
    }
}

impl WorkerTelemetry {
    pub fn capture() -> Self {
        Self {
            owner: ComponentObservations::current_or_disabled(),
            scope: ExecutionScope::capture().with_context(ExecutionContext::default()),
        }
    }

    pub(crate) fn bind_queries(&self, queries: &[String]) -> Result<(), TelemetryError> {
        self.owner.bind_reaction_queries(queries)
    }

    /// Begin actual effect work, separately from trace/checkpoint scope.
    pub fn reaction_work(&self) -> ReactionWork {
        if self.owner.telemetry().handled_branch_tracker().is_none() {
            return ReactionWork::new(self.owner.clone(), ObservedOperation::Reaction);
        }
        self.reaction_work_context(&ExecutionContext::current())
    }

    pub fn reaction_work_context(&self, context: &ExecutionContext) -> ReactionWork {
        let mut work = ReactionWork::new(self.owner.clone(), ObservedOperation::Reaction);
        work.timing = self.owner.handled_branch(context);
        work.query = context
            .handled_output()
            .map(|output| output.query_id.clone());
        work
    }

    /// Preserve local work counters for enqueue/forwarding adapters without
    /// presenting their successful acceptance as source-to-handled completion.
    pub fn reaction_acceptance_work(&self) -> ReactionWork {
        ReactionWork::new(self.owner.clone(), ObservedOperation::Reaction)
    }

    /// Call after collecting a batch, immediately before its actual handling.
    pub fn reaction_batch_work(&self) -> ReactionWork {
        ReactionWork::new(self.owner.clone(), ObservedOperation::ReactionBatch)
    }

    /// Keep with the existing buffered items when one input spans several batches.
    pub fn reaction_input(&self, query: &str, rows: usize) -> ReactionInput {
        self.reaction_input_inner(query, rows, None)
    }

    /// Original per-envelope provenance must accompany split/batched rows.
    pub fn reaction_input_context(
        &self,
        query: &str,
        rows: usize,
        context: &ExecutionContext,
    ) -> ReactionInput {
        let timing = context
            .handled_output()
            .filter(|output| output.query_id.as_ref() == query && output.required_rows == rows)
            .and_then(|_| self.owner.handled_branch(context));
        self.reaction_input_inner(query, rows, timing)
    }

    fn reaction_input_inner(
        &self,
        query: &str,
        rows: usize,
        timing: Option<super::handled_branch::HandledBranchObservation>,
    ) -> ReactionInput {
        let subscription = self.owner.reaction_subscription(query);
        if rows > 0 && subscription.is_none() && self.owner.configured() {
            log::warn!("No configured reaction metric subscription for query '{query}'");
        }
        ReactionInput(subscription.filter(|_| rows > 0).map(|subscription| {
            Arc::new(InputProgress {
                subscription,
                remaining: AtomicUsize::new(rows),
                timing,
            })
        }))
    }

    pub(crate) fn checkpoint(&self, query: &str, sequence: u64, hash: u64) {
        if let Some(subscription) = self.owner.reaction_subscription(query) {
            if let Err(error) = subscription.checkpoint(sequence, hash) {
                log::warn!("Cannot observe reaction checkpoint for '{query}': {error}");
            }
        }
    }

    pub(crate) fn checkpoint_failed(&self) {
        self.owner.checkpoint_failed();
    }

    pub(crate) fn subscription(
        &self,
        query: &str,
    ) -> Option<Arc<super::ReactionSubscriptionMetrics>> {
        self.owner.reaction_subscription(query)
    }

    pub async fn scope<T>(&self, future: impl Future<Output = T>) -> T {
        self.owner.scope(self.scope.scope(future)).await
    }

    pub async fn scope_context<T>(
        &self,
        context: ExecutionContext,
        future: impl Future<Output = T>,
    ) -> T {
        self.owner
            .scope(self.scope.with_context(context).scope(future))
            .await
    }

    pub fn reaction(
        &self,
        context: &ExecutionContext,
        change_count: usize,
    ) -> Result<WorkerObservation, TelemetryError> {
        self.scope.in_scope(|| {
            self.owner
                .begin(
                    ObservedOperation::Reaction,
                    context,
                    OperationFields {
                        input_count: Some(1),
                        change_count: Some(change_count),
                        completion: Some("handled"),
                        ..Default::default()
                    },
                )
                .map(WorkerObservation)
        })
    }

    pub fn reaction_batch<'a>(
        &self,
        contexts: impl IntoIterator<Item = &'a ExecutionContext>,
        input_count: usize,
        change_count: usize,
    ) -> Result<WorkerObservation, TelemetryError> {
        self.scope.in_scope(|| {
            self.owner
                .batch_operation(
                    ObservedOperation::ReactionBatch,
                    contexts,
                    OperationFields {
                        input_count: Some(input_count),
                        change_count: Some(change_count),
                        completion: Some("handled"),
                        ..Default::default()
                    },
                )
                .map(WorkerObservation)
        })
    }

    /// Call inside the receiving component's scope for each actual service unit.
    pub fn service_work(
        &self,
        context: &ExecutionContext,
    ) -> Result<WorkerObservation, TelemetryError> {
        self.scope.in_scope(|| {
            self.owner
                .begin(ObservedOperation::ServiceWork, context, Default::default())
                .map(WorkerObservation)
        })
    }
}

/// A worker trace; successful effects are accounted separately by `ReactionWork`.
pub struct WorkerObservation(OperationObservation);

impl WorkerObservation {
    pub async fn run<T, E>(self, future: impl Future<Output = Result<T, E>>) -> Result<T, E> {
        self.0.run(future).await
    }

    pub async fn run_infallible<T>(self, future: impl Future<Output = T>) -> T {
        self.run(async { Ok::<_, std::convert::Infallible>(future.await) })
            .await
            .unwrap_or_else(|never| match never {})
    }
}

/// One local handling attempt. Checkpointing must happen after `finish`.
pub struct ReactionWork {
    owner: Arc<ComponentObservations>,
    operation: ObservedOperation,
    started: Option<Instant>,
    finished: bool,
    timing: Option<super::handled_branch::HandledBranchObservation>,
    query: Option<Arc<str>>,
}

impl ReactionWork {
    fn new(owner: Arc<ComponentObservations>, operation: ObservedOperation) -> Self {
        let started = owner.start_work(operation);
        Self {
            owner,
            operation,
            started,
            finished: false,
            timing: None,
            query: None,
        }
    }

    /// Report positively completed rows and whole input envelopes, before checkpointing.
    pub fn handled(&self, query: &str, rows: usize, inputs: usize) {
        if let Some(subscription) = self.owner.reaction_subscription(query) {
            subscription.handled(rows, inputs);
        } else if self.owner.configured() {
            log::warn!("No configured reaction metric subscription for query '{query}'");
        }
        if inputs == 1 && self.query.as_deref() == Some(query) {
            if let Some(timing) = &self.timing {
                timing.finish(None);
            }
        }
    }

    pub fn finish(&mut self, success: bool) {
        if !self.finished {
            self.finished = true;
            self.owner
                .finish_work(self.operation, self.started, usize::from(!success));
        }
    }
}

impl Drop for ReactionWork {
    fn drop(&mut self) {
        if !self.finished {
            self.owner.finish_work(self.operation, self.started, 2);
        }
    }
}

#[derive(Debug)]
struct InputProgress {
    subscription: Arc<super::ReactionSubscriptionMetrics>,
    remaining: AtomicUsize,
    timing: Option<super::handled_branch::HandledBranchObservation>,
}

/// Transient completion accounting, bounded by the worker's existing queue/batch.
#[derive(Clone, Debug, Default)]
pub struct ReactionInput(Option<Arc<InputProgress>>);

impl ReactionInput {
    /// Complete the original envelope after positively handling every required
    /// row. Does not change existing counters; report those through ReactionWork.
    pub fn completed(&self) {
        if let Some(timing) = self.0.as_ref().and_then(|input| input.timing.as_ref()) {
            timing.finish(None);
        }
    }

    /// Report stable ordinals in the original non-Noop row set. Existing work
    /// counters retain their attempt semantics; latency completion is idempotent.
    pub fn handled_rows(&self, ordinals: &[usize]) {
        self.handled(ordinals.len());
        self.handled_timing_rows(ordinals);
    }

    /// For adapters whose existing work-counter unit differs from original rows.
    pub fn handled_timing_rows(&self, ordinals: &[usize]) {
        if let Some(timing) = self.0.as_ref().and_then(|input| input.timing.as_ref()) {
            timing.finish(Some(ordinals));
        }
    }

    pub fn handled(&self, rows: usize) {
        let Some(input) = &self.0 else {
            return;
        };
        if rows == 0 {
            return;
        }
        match input
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(rows)
            }) {
            Ok(remaining) => input
                .subscription
                .handled(rows, usize::from(remaining == rows)),
            Err(_) => {
                log::error!("Reaction metric completion exceeded the input's row count")
            }
        }
    }
}
