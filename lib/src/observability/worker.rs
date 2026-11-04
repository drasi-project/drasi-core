// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    ComponentObservations, ExecutionContext, ExecutionScope, ObservedOperation, OperationFields,
    OperationObservation, TelemetryError,
};
use std::{future::Future, sync::Arc};

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

/// A trace-only worker operation; it does not upgrade graph acceptance.
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
