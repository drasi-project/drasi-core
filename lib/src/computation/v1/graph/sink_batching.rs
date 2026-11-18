// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::sink_delivery::{self, FailureAction, SinkDeliveryStatus};
use super::*;
use crate::computation::v1::{
    Acknowledgement, ComponentOperationError, FailureDisposition, HandlingOutcome,
    PermanentFailurePolicy, SinkBatching,
};
use tokio::time::Instant;

#[cfg(test)]
mod tests;

struct RetainedInput {
    edge: usize,
    producer: super::super::ProducerBoundary,
    input: InputEnvelope,
    acknowledgement: Option<Box<dyn Acknowledgement>>,
    _work: DeliveryWork,
    bytes: usize,
}

/// Lives on the graph's instance lease, never solely in a cancellable future.
#[derive(Default)]
pub(super) struct SinkBatchState {
    inputs: VecDeque<RetainedInput>,
    ready: VecDeque<usize>,
    deadline: Option<Instant>,
    flushing: bool,
    completed: usize,
    acknowledging: Option<BoxFuture<'static, Result<(), PipeError>>>,
    acknowledgement_failed: bool,
    /// Failed attempts of the input at the head of the batch.
    attempts: u32,
    /// Inputs to deliver one at a time to isolate a permanently failing one.
    isolate: usize,
}

/// Whether a flush delivered every retained input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flushed {
    Complete,
    /// Stop or pause arrived while an input awaited retry; it stays retained.
    Interrupted,
}

/// What follows a failed delivery attempt.
enum Settled {
    Retry,
    Skipped,
    Interrupted,
}

impl SinkBatchState {
    pub(super) fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    fn push(&mut self, incoming: &mut Incoming, node: &NodeSnapshot) -> GraphResult<()> {
        let PendingInput { delivery, work } = incoming.pending.take().expect("ready input");
        let (envelope, acknowledgement) = delivery.into_parts();
        let matches_acknowledgement =
            acknowledgement.is_some() == incoming.acknowledgement_required;
        let input = InputEnvelope {
            port: incoming.port.clone(),
            envelope,
        };
        // Retain ownership before any validation that may fail.
        self.inputs.push_back(RetainedInput {
            edge: incoming.edge,
            producer: incoming.producer,
            input,
            acknowledgement,
            _work: work,
            bytes: 0,
        });
        let retained = self.inputs.back_mut().expect("retained input");
        if !matches_acknowledgement {
            return Err(topology(
                "pipe delivery acknowledgement differs from its negotiated capability",
            ));
        }
        if retained.acknowledgement.is_some() && node.completion == Some(SinkCompletion::Accepted) {
            return Err(topology(
                "acceptance-only sink cannot acknowledge completed handling",
            ));
        }
        let port = node
            .descriptor
            .ports()
            .iter()
            .find(|port| port.id() == &retained.input.port)
            .ok_or_else(|| topology("delivery targets an undeclared input port"))?;
        validate_schema(port.schema(), retained.input.envelope.changes().schema())?;
        retained.bytes = SinkBatching::input_size(&retained.input)
            .map_err(|error| component_error(node, "measure sink input", error))?;
        Ok(())
    }

    fn operations(&self) -> usize {
        self.inputs
            .iter()
            .map(|input| input.input.envelope.changes().operations().len())
            .fold(0usize, usize::saturating_add)
    }

    fn bytes(&self) -> usize {
        self.inputs
            .iter()
            .map(|input| input.bytes)
            .fold(5usize, usize::saturating_add)
    }

    fn should_flush(&self, policy: Option<SinkBatching>) -> bool {
        let Some(policy) = policy else {
            return !self.inputs.is_empty();
        };
        self.flushing
            || self.completed > 0
            || self.inputs.len() >= policy.max_inputs
            || self.operations() >= policy.max_operations
            || self.bytes() >= policy.max_bytes
            || self
                .inputs
                .front()
                .is_some_and(|input| SinkBatching::is_barrier(&input.input))
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn crosses_boundary(
        &self,
        candidate: &InputEnvelope,
        policy: SinkBatching,
    ) -> anyhow::Result<bool> {
        let Some(first) = self.inputs.front() else {
            return Ok(false);
        };
        let bytes = SinkBatching::input_size(candidate)?;
        Ok(first.input.port != candidate.port
            || SinkBatching::is_barrier(candidate)
            || self.bytes().saturating_add(bytes) > policy.max_bytes
            || self
                .operations()
                .saturating_add(candidate.envelope.changes().operations().len())
                > policy.max_operations)
    }

    async fn acknowledge(&mut self) -> GraphResult<()> {
        if self.acknowledgement_failed {
            return Err(topology(
                "failed sink batch acknowledgement requires pipe reconstruction",
            ));
        }
        while self.completed > 0 {
            let first = self.inputs.front_mut().expect("completed input");
            if self.acknowledging.is_none() {
                self.acknowledging = first
                    .acknowledgement
                    .take()
                    .map(|acknowledgement| acknowledgement.complete(HandlingOutcome::Handled));
            }
            if let Some(acknowledging) = &mut self.acknowledging {
                let result = acknowledging.await;
                self.acknowledging = None;
                if let Err(source) = result {
                    self.acknowledgement_failed = true;
                    return Err(GraphError::Pipe {
                        edge: first.edge,
                        source,
                    });
                }
            }
            self.inputs.pop_front();
            self.completed -= 1;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn flush(
        &mut self,
        component: &mut Component,
        node: &NodeSnapshot,
        input_progress: &mut Option<Box<input_progress::InputProgress>>,
        quiesce: &mut watch::Receiver<bool>,
        delivery: &SinkDeliveryStatus,
    ) -> GraphResult<Flushed> {
        self.flushing = true;
        self.acknowledge().await?;
        while !self.inputs.is_empty() {
            let Component::Sink(sink) = component else {
                return Err(topology("retained sink batch belongs to a non-sink"));
            };
            let policy = sink
                .batching()
                .map_err(|error| component_error(node, "sink batch policy", error))?;
            if let Some(policy) = policy {
                policy
                    .validate()
                    .map_err(|error| component_error(node, "sink batch policy", error))?;
            }
            let first = self.inputs.front().expect("nonempty batch");
            let use_batch = self.isolate == 0
                && policy.is_some_and(|policy| {
                    !SinkBatching::is_barrier(&first.input)
                        && first.bytes.saturating_add(5) <= policy.max_bytes
                });
            let count = if use_batch {
                let policy = policy.expect("batch policy");
                let mut bytes = 5usize;
                let mut operations = 0usize;
                let mut count = 0;
                for retained in self.inputs.iter().take(policy.max_inputs) {
                    if retained.input.port != first.input.port
                        || SinkBatching::is_barrier(&retained.input)
                        || bytes.saturating_add(retained.bytes) > policy.max_bytes
                        || (count > 0
                            && operations.saturating_add(
                                retained.input.envelope.changes().operations().len(),
                            ) > policy.max_operations)
                    {
                        break;
                    }
                    bytes += retained.bytes;
                    operations = operations
                        .saturating_add(retained.input.envelope.changes().operations().len());
                    count += 1;
                }
                count
            } else {
                1
            };
            let prepared = input_progress
                .as_ref()
                .map(|progress| {
                    progress.prepare_batch(
                        self.inputs
                            .iter()
                            .take(count)
                            .map(|input| (&input.input.envelope, input.producer)),
                    )
                })
                .transpose()
                .map_err(|error| component_error(node, "prepare batch input progress", error))?;
            let is_new = |index: usize| {
                prepared
                    .as_ref()
                    .is_none_or(|inputs| !inputs[index].duplicate)
            };
            let (operation, result) = if use_batch {
                let batch: Vec<_> = self
                    .inputs
                    .iter()
                    .take(count)
                    .enumerate()
                    .filter(|(index, _)| is_new(*index))
                    .map(|(_, input)| input.input.clone())
                    .collect();
                let result = if batch.is_empty() {
                    Ok(())
                } else {
                    sink.handle_batch(batch).await
                };
                ("handle batch", result)
            } else if is_new(0) {
                ("handle", sink.handle(first.input.clone()).await)
            } else {
                ("handle", Ok(()))
            };
            match result {
                Ok(()) => {
                    if std::mem::take(&mut self.attempts) > 0 {
                        delivery.recovered();
                    }
                }
                Err(error) => {
                    match self
                        .settle(component, node, operation, error, count, quiesce, delivery)
                        .await?
                    {
                        Settled::Retry => continue,
                        Settled::Interrupted => return Ok(Flushed::Interrupted),
                        Settled::Skipped => {}
                    }
                }
            }
            self.isolate = self.isolate.saturating_sub(count);
            check_descriptor(component, node)?;
            if let (Some(progress), Some(prepared)) = (input_progress.as_mut(), prepared) {
                progress.confirm(prepared);
            }
            self.completed = count;
            self.acknowledge().await?;
        }
        self.flushing = false;
        self.deadline = None;
        Ok(Flushed::Complete)
    }

    /// Applies the delivery-failure model to one failed attempt of the
    /// `count` inputs at the head of the batch.
    #[allow(clippy::too_many_arguments)]
    async fn settle(
        &mut self,
        component: &Component,
        node: &NodeSnapshot,
        operation: &'static str,
        error: anyhow::Error,
        count: usize,
        quiesce: &mut watch::Receiver<bool>,
        delivery: &SinkDeliveryStatus,
    ) -> GraphResult<Settled> {
        let id = node.descriptor.id();
        match sink_delivery::classify(&error) {
            FailureAction::Fail => Err(component_error(node, operation, error)),
            FailureAction::Permanent if count > 1 => {
                // Deliver these inputs one at a time so that only the input
                // that can never be delivered is subject to the policy.
                self.isolate = count;
                Ok(Settled::Retry)
            }
            FailureAction::Permanent => {
                match sink_delivery::permanent_policy(component.description()) {
                    PermanentFailurePolicy::Stall => {
                        log::error!(
                            "Sink {id} stalled on an input that can never be delivered: {error:#}"
                        );
                        Err(component_error(
                            node,
                            operation,
                            ComponentOperationError::new(FailureDisposition::Terminal, error)
                                .into(),
                        ))
                    }
                    PermanentFailurePolicy::Skip => {
                        log::warn!(
                            "Sink {id} skipped an input that can never be delivered: {error:#}"
                        );
                        if std::mem::take(&mut self.attempts) > 0 {
                            delivery.recovered();
                        }
                        delivery.skipped();
                        Ok(Settled::Skipped)
                    }
                }
            }
            FailureAction::Retry(retry_after) => {
                self.attempts = self.attempts.saturating_add(1);
                let delay = sink_delivery::backoff(self.attempts, retry_after);
                log::warn!(
                    "Sink {id} delivery attempt {} failed; retrying the same input in {delay:?}: {error:#}",
                    self.attempts
                );
                delivery.retrying(component_error(node, operation, error));
                if *quiesce.borrow() {
                    return Ok(Settled::Interrupted);
                }
                tokio::select! {
                    biased;
                    _ = cancelled(quiesce) => Ok(Settled::Interrupted),
                    _ = tokio::time::sleep(delay) => Ok(Settled::Retry),
                }
            }
        }
    }
}

async fn next_input(
    inputs: &mut [Incoming],
    merge: InputMergePolicy,
    ready: &mut VecDeque<usize>,
) -> GraphResult<Option<usize>> {
    let positions: BTreeMap<_, _> = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| (input.edge, index))
        .collect();
    ready.retain(|edge| {
        positions
            .get(edge)
            .is_some_and(|index| inputs[*index].pending.is_some())
    });
    let known: BTreeSet<_> = ready.iter().copied().collect();
    for input in inputs.iter().filter(|input| input.pending.is_some()) {
        if !known.contains(&input.edge) {
            ready.push_back(input.edge);
        }
    }
    {
        let mut receivers: FuturesUnordered<_> = inputs
            .iter_mut()
            .filter(|input| !input.exhausted && input.pending.is_none())
            .map(receive)
            .collect();
        if ready.is_empty() {
            while let Some((incoming, result)) = receivers.next().await {
                result.map_err(|source| GraphError::Pipe {
                    edge: incoming.edge,
                    source,
                })?;
                if incoming.pending.is_some() {
                    ready.push_back(incoming.edge);
                    break;
                }
            }
        }
        if merge == InputMergePolicy::EventTimeAcrossStreams {
            while let Some(Some((incoming, result))) = receivers.next().now_or_never() {
                result.map_err(|source| GraphError::Pipe {
                    edge: incoming.edge,
                    source,
                })?;
                if incoming.pending.is_some() {
                    ready.push_back(incoming.edge);
                }
            }
        }
    }
    if ready.is_empty() {
        return Ok(None);
    }
    let chosen = if merge == InputMergePolicy::EventTimeAcrossStreams {
        event_time_selection(ready.iter().map(|edge| {
            inputs[positions[edge]]
                .pending
                .as_ref()
                .expect("ready")
                .delivery
                .envelope()
                .system()
                .timestamp()
        }))
    } else {
        0
    };
    Ok(Some(positions[&ready[chosen]]))
}

pub(super) async fn run(
    component: &mut Component,
    node: &NodeSnapshot,
    inputs: &mut [Incoming],
    quiesce: &mut watch::Receiver<bool>,
    budget: &NodeWorkBudget,
    retained: &mut Option<Box<SinkBatchState>>,
    input_progress: &mut Option<Box<input_progress::InputProgress>>,
    delivery: &SinkDeliveryStatus,
) -> GraphResult<()> {
    if inputs.is_empty() && retained.as_ref().is_none_or(|state| state.is_empty()) {
        cancelled(quiesce).await;
        return Ok(());
    }
    let state = retained.get_or_insert_with(|| Box::new(SinkBatchState::default()));
    let activity = super::super::feedback::Activity::current();
    loop {
        check_descriptor(component, node)?;
        let Component::Sink(sink) = component else {
            return Err(topology("batch processing requires a sink"));
        };
        let policy = sink
            .batching()
            .map_err(|error| component_error(node, "sink batch policy", error))?;
        if let Some(policy) = policy {
            policy
                .validate()
                .map_err(|error| component_error(node, "sink batch policy", error))?;
        }
        if *quiesce.borrow() {
            if !state.is_empty() {
                state
                    .flush(component, node, input_progress, quiesce, delivery)
                    .await?;
            }
            return Ok(());
        }
        if state.should_flush(policy)
            || (!state.is_empty()
                && activity
                    .as_ref()
                    .is_some_and(|activity| activity.draining()))
        {
            if state
                .flush(component, node, input_progress, quiesce, delivery)
                .await?
                == Flushed::Interrupted
            {
                return Ok(());
            }
            continue;
        }
        if state.is_empty()
            && activity.is_none()
            && inputs.len() == 1
            && inputs[0].pending.is_none()
            && !inputs[0].exhausted
            && inputs[0].receiver.supports_batch_receive()
        {
            if let Some(policy) = policy {
                state.ready.clear();
                let incoming = &mut inputs[0];
                let received = tokio::select! {
                    biased;
                    _ = cancelled(quiesce) => None,
                    result = incoming.receiver.receive_batch(policy, &incoming.port) =>
                        Some(result.map_err(|source| GraphError::Pipe { edge: incoming.edge, source })?),
                };
                let deliveries = match received {
                    Some(deliveries) => {
                        incoming.exhausted = deliveries.is_empty();
                        deliveries
                    }
                    None => incoming.receiver.take_pending_batch().map_err(|source| {
                        GraphError::Pipe {
                            edge: incoming.edge,
                            source,
                        }
                    })?,
                };
                let count = deliveries.len();
                for delivery in deliveries {
                    incoming.pending = Some(PendingInput {
                        delivery,
                        work: DeliveryWork::new(incoming.progress.clone())?,
                    });
                    state.push(incoming, node)?;
                }
                // Stash every receipt before the first yield or network await.
                state.flushing = count > 0;
                for _ in 0..count {
                    if let Some(yield_node) = budget.checkpoint() {
                        yield_node.await;
                    }
                }
                if incoming.exhausted {
                    return Ok(());
                }
                continue;
            }
        }
        let deadline = state.deadline;
        let selected = tokio::select! {
            biased;
            _ = cancelled(quiesce) => continue,
            _ = async { activity.as_ref().expect("feedback enabled").wait_draining().await },
                if activity.is_some() && !state.is_empty() => continue,
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => continue,
            result = next_input(inputs, node.input_merge, &mut state.ready) => result?,
        };
        let Some(selected) = selected else {
            if !state.is_empty() {
                state
                    .flush(component, node, input_progress, quiesce, delivery)
                    .await?;
            }
            return Ok(());
        };
        let incoming = &mut inputs[selected];
        let candidate = InputEnvelope {
            port: incoming.port.clone(),
            envelope: incoming
                .pending
                .as_ref()
                .expect("ready input")
                .delivery
                .envelope()
                .clone(),
        };
        if let Some(policy) = policy {
            if state
                .crosses_boundary(&candidate, policy)
                .map_err(|error| component_error(node, "measure sink input", error))?
            {
                if state
                    .flush(component, node, input_progress, quiesce, delivery)
                    .await?
                    == Flushed::Interrupted
                {
                    return Ok(());
                }
                continue;
            }
        }
        state.push(incoming, node)?;
        state.ready.retain(|edge| *edge != incoming.edge);
        if state.deadline.is_none() {
            state.deadline =
                policy.map(|policy| Instant::now() + Duration::from_millis(policy.max_delay_ms));
        }
        if let Some(yield_node) = budget.checkpoint() {
            yield_node.await;
        }
    }
}
