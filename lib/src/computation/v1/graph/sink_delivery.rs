// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! One delivery-failure model for every sink node.
//!
//! A failed `handle` is classified by the [`DeliveryFailure`] in its chain:
//! transient failures (and unclassified errors) are retried in place, with the
//! same input, using bounded exponential backoff with jitter; permanent
//! failures follow the sink's declared [`PermanentFailurePolicy`]. A panic,
//! interruption or a terminal lifecycle error still fails the node, because
//! the activation itself can no longer deliver.
//!
//! While a sink retries, its observed component stays `Running`, its health is
//! `Degraded` and its failure is the last error as a retryable processing
//! failure. Skipped inputs are counted in `ObservedComponent::skipped_inputs`.
//! The node publishes both through the graph's metadata notification, which
//! the controller applies to the observed graph; no extra task is involved.

use std::{
    hash::BuildHasher,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Utc;
use drasi_computation_contracts::{delivery_failure_class, PermanentFailurePolicy};

use super::{
    controller::update, registry::MetadataChanges, ComponentGeneration, ComponentId,
    ComputationGraph, GraphError,
};
use crate::computation::v1::{
    ComponentFailure, ComponentHealth, ComponentLifecycle, FailureDisposition, FailurePhase,
    LifecycleError, LifecycleErrorKind,
};

/// First retry delay; each further attempt doubles it up to [`MAX_DELAY`].
pub(super) const INITIAL_DELAY: Duration = Duration::from_millis(200);
/// Largest backoff delay between attempts of one input.
pub(super) const MAX_DELAY: Duration = Duration::from_secs(30);
/// Largest destination retry-after hint the graph honours.
pub(super) const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);

/// What the node does with a failed delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureAction {
    /// Retry the same input after at least the given hint.
    Retry(Option<Duration>),
    /// Apply the declared permanent-failure policy.
    Permanent,
    /// The activation cannot deliver more input: fail the node.
    Fail,
}

pub(super) fn classify(error: &anyhow::Error) -> FailureAction {
    if error.chain().take(64).any(|cause| {
        cause.is::<crate::computation::v1::ComponentPanic>()
            || cause.is::<crate::computation::v1::ComponentInterrupted>()
            || cause.downcast_ref::<LifecycleError>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    LifecycleErrorKind::Terminal | LifecycleErrorKind::Unsupported
                )
            })
    }) {
        return FailureAction::Fail;
    }
    match delivery_failure_class(error) {
        Some(class) if class.is_permanent() => FailureAction::Permanent,
        Some(class) => FailureAction::Retry(class.retry_after()),
        None => FailureAction::Retry(None),
    }
}

/// The delay before retry `attempt` (1-based): exponential from
/// [`INITIAL_DELAY`], capped at [`MAX_DELAY`], with jitter in the upper half
/// so retrying sinks do not synchronise, and never shorter than a
/// destination's (bounded) retry-after hint.
pub(super) fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let ceiling = INITIAL_DELAY.saturating_mul(1 << exponent).min(MAX_DELAY);
    let jitter = std::hash::RandomState::new().hash_one(attempt) % 1024;
    let delay = ceiling / 2 + (ceiling / 2).mul_f64(jitter as f64 / 1023.0);
    delay.max(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER))
}

/// The permanent-failure policy a sink declared in its description.
pub(super) fn permanent_policy(
    description: Option<drasi_computation_contracts::ComponentDescription>,
) -> PermanentFailurePolicy {
    description
        .map(|description| description.component.on_permanent_failure)
        .unwrap_or_default()
}

#[derive(Default)]
struct Status {
    retrying: Option<Arc<GraphError>>,
    recovered: Option<Arc<GraphError>>,
    skipped: u64,
    changed: bool,
}

/// A sink node's delivery status, owned by its construction generation and
/// published into the observed graph by the controller.
pub(super) struct SinkDeliveryStatus {
    id: ComponentId,
    generation: ComponentGeneration,
    changes: MetadataChanges,
    status: Mutex<Status>,
}

impl SinkDeliveryStatus {
    pub(super) fn new(
        id: ComponentId,
        generation: ComponentGeneration,
        changes: MetadataChanges,
    ) -> Self {
        Self {
            id,
            generation,
            changes,
            status: Mutex::new(Status::default()),
        }
    }

    fn change(&self, change: impl FnOnce(&mut Status) -> bool) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if change(&mut status) {
            status.changed = true;
            drop(status);
            self.changes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(self.id.clone());
            self.changes.notify();
        }
    }

    /// The node is retrying an input after `cause`.
    pub(super) fn retrying(&self, cause: GraphError) {
        self.change(|status| {
            status.retrying = Some(Arc::new(cause));
            true
        });
    }

    /// The input that was being retried has been delivered.
    pub(super) fn recovered(&self) {
        self.change(|status| {
            status.recovered = status.retrying.take();
            status.recovered.is_some()
        });
    }

    /// One more input was skipped after a permanent failure.
    pub(super) fn skipped(&self) {
        self.change(|status| {
            status.skipped = status.skipped.saturating_add(1);
            true
        });
    }

    fn take(&self) -> Option<(Option<Arc<GraphError>>, Option<Arc<GraphError>>, u64)> {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !std::mem::take(&mut status.changed) {
            return None;
        }
        Some((
            status.retrying.clone(),
            status.recovered.take(),
            status.skipped,
        ))
    }
}

/// Publish changed component metadata, first applying changed sink delivery
/// statuses to the observed graph.
pub(super) fn publish(graph: &ComputationGraph) {
    let changed: Vec<ComponentId> = graph
        .metadata_changes
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .cloned()
        .collect();
    let statuses: Vec<_> = changed
        .iter()
        .filter_map(|id| graph.ids.get(id))
        .filter_map(|index| {
            let status = graph.components[*index].delivery();
            status
                .take()
                .map(|change| (status.id.clone(), status.generation, change))
        })
        .collect();
    if statuses.is_empty() {
        super::registry::publish(graph);
        return;
    }
    update(graph, |state| {
        for (id, generation, (retrying, recovered, skipped)) in statuses {
            let Some(node) = state.components.get_mut(&id) else {
                continue;
            };
            if node.generation != generation {
                continue;
            }
            node.skipped_inputs = skipped;
            if let Some(recovered) = recovered {
                if node
                    .failure
                    .as_ref()
                    .is_some_and(|failure| Arc::ptr_eq(&failure.cause, &recovered))
                {
                    node.failure = None;
                    node.health = ComponentHealth::Healthy;
                }
            }
            if let Some(cause) = retrying {
                let replaceable = node.failure.as_ref().is_none_or(|failure| {
                    failure.phase == FailurePhase::Processing
                        && failure.disposition == FailureDisposition::Retryable
                });
                if replaceable
                    && matches!(
                        node.lifecycle,
                        ComponentLifecycle::Running | ComponentLifecycle::Quiescing
                    )
                {
                    node.failure = Some(ComponentFailure {
                        phase: FailurePhase::Processing,
                        disposition: FailureDisposition::Retryable,
                        cause,
                        timestamp: Utc::now(),
                    });
                    node.health = ComponentHealth::Degraded;
                }
            }
            node.transition_time = Utc::now();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_is_bounded_and_honours_bounded_hints() {
        for attempt in 1u32..40 {
            let ceiling = INITIAL_DELAY
                .saturating_mul(1 << attempt.saturating_sub(1).min(16))
                .min(MAX_DELAY);
            let delay = backoff(attempt, None);
            assert!(
                delay >= ceiling / 2 && delay <= ceiling,
                "{attempt}: {delay:?}"
            );
        }
        assert_eq!(
            backoff(1, Some(Duration::from_secs(20))),
            Duration::from_secs(20)
        );
        assert_eq!(
            backoff(1, Some(Duration::from_secs(86_400))),
            MAX_RETRY_AFTER
        );
    }

    #[test]
    fn unclassified_errors_retry_and_only_activation_damage_fails_the_node() {
        use drasi_computation_contracts::DeliveryFailure;
        assert_eq!(classify(&anyhow::anyhow!("io")), FailureAction::Retry(None));
        assert_eq!(
            classify(&DeliveryFailure::permanent(anyhow::anyhow!("rejected")).into()),
            FailureAction::Permanent
        );
        assert_eq!(
            classify(&LifecycleError::terminal(anyhow::anyhow!("cleanup required")).into()),
            FailureAction::Fail
        );
        assert_eq!(
            classify(
                &crate::computation::v1::ComponentPanic::new(DeliveryFailure::transient(
                    anyhow::anyhow!("panicked")
                ))
                .into()
            ),
            FailureAction::Fail
        );
    }
}
