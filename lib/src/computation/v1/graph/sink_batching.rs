// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{Acknowledgement, HandlingOutcome, SinkBatching};
use tokio::time::Instant;

#[cfg(test)]
mod tests;

struct RetainedInput {
    edge: usize,
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

    async fn flush(&mut self, component: &mut Component, node: &NodeSnapshot) -> GraphResult<()> {
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
            let use_batch = policy.is_some_and(|policy| {
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
                let batch = self
                    .inputs
                    .iter()
                    .take(count)
                    .map(|input| input.input.clone())
                    .collect();
                sink.handle_batch(batch)
                    .await
                    .map_err(|error| component_error(node, "handle batch", error))?;
                count
            } else {
                sink.handle(first.input.clone())
                    .await
                    .map_err(|error| component_error(node, "handle", error))?;
                1
            };
            check_descriptor(component, node)?;
            self.completed = count;
            self.acknowledge().await?;
        }
        self.flushing = false;
        self.deadline = None;
        Ok(())
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
) -> GraphResult<()> {
    if inputs.is_empty() && retained.as_ref().map_or(true, |state| state.is_empty()) {
        cancelled(quiesce).await;
        return Ok(());
    }
    let state = retained.get_or_insert_with(|| Box::new(SinkBatchState::default()));
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
                state.flush(component, node).await?;
            }
            return Ok(());
        }
        if state.should_flush(policy) {
            state.flush(component, node).await?;
            continue;
        }
        if state.is_empty()
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
                state.flush(component, node).await?;
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
                state.flush(component, node).await?;
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
