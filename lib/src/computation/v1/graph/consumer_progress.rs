// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{topology, GraphResult, GraphSnapshot};
use crate::computation::v1::{
    recovery::ProcessingRecovery, ComponentId, ComponentRecovery, ComponentRole,
    ConsumerProgressSet, GraphError,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Default)]
pub(super) struct Validation {
    pending: BTreeSet<ComponentId>,
    failure: Option<GraphError>,
}

impl Validation {
    pub(super) fn wait_for(&mut self, component: ComponentId) {
        self.pending.insert(component);
    }

    pub(super) fn failed(error: GraphError) -> Self {
        Self {
            pending: BTreeSet::new(),
            failure: Some(error),
        }
    }

    pub(super) fn waiting_for(&self) -> BTreeSet<ComponentId> {
        // Invalid bindings must reach activation so their failure is recorded,
        // rather than masquerading as a dependency that can arrive later.
        if self.failure.is_some() {
            BTreeSet::new()
        } else {
            self.pending.clone()
        }
    }

    pub(super) fn validate(self, allow_pending: bool) -> GraphResult<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !allow_pending && !self.pending.is_empty() {
            return Err(topology(format!(
                "consumer progress is waiting for actual owners or their data paths: {:?}",
                self.pending
            )));
        }
        Ok(())
    }
}

pub(super) fn bind_positions(
    contract: &ComponentRecovery,
    snapshot: &GraphSnapshot,
) -> GraphResult<()> {
    let Some(progress) = &contract.consumer_progress else {
        return Ok(());
    };
    let policy = contract.consumer_positions.clone();
    let role = snapshot
        .nodes
        .iter()
        .find(|node| node.descriptor.id() == progress.producer_id())
        .ok_or_else(|| topology("input replay producer is not declared"))?
        .role;
    let mut reached = BTreeSet::new();
    let mut visited = BTreeSet::from([progress.producer_id().clone()]);
    let mut pending = VecDeque::from([(progress.producer_id().clone(), Vec::new())]);
    while let Some((producer, path)) = pending.pop_front() {
        let node = snapshot
            .nodes
            .iter()
            .find(|node| node.descriptor.id() == &producer)
            .ok_or_else(|| topology("input replay path has no actual producer"))?;
        for edge in snapshot
            .edges
            .iter()
            .filter(|edge| edge.definition.from.component == producer)
        {
            let stream = node
                .output_streams
                .get(&edge.definition.from.port)
                .ok_or_else(|| topology("input replay path has no bound producer stream"))?;
            let mut next = path.clone();
            next.push(stream.clone());
            let consumer = &edge.definition.to.component;
            if let Some(owner) = progress.owners().get(consumer) {
                if !reached.insert(consumer.clone()) {
                    return Err(topology("input replay owner has multiple producer paths"));
                }
                owner
                    .bind_input_boundary(progress.producer_id(), role, policy.clone(), next)
                    .map_err(|error| {
                        topology(format!("invalid input replay position binding: {error:#}"))
                    })?;
            } else {
                if !visited.insert(consumer.clone()) {
                    return Err(topology("input replay producer path is ambiguous"));
                }
                pending.push_back((consumer.clone(), next));
            }
        }
    }
    if reached.len() != progress.owners().len() {
        return Err(topology(
            "input replay policies require actual bound data paths to every consumer",
        ));
    }
    Ok(())
}

pub(super) fn participants(
    snapshot: &GraphSnapshot,
    progress: &ConsumerProgressSet,
) -> BTreeSet<ComponentId> {
    let mut selected = BTreeSet::from([progress.producer_id().clone()]);
    let mut pending = VecDeque::from([progress.producer_id().clone()]);
    while let Some(id) = pending.pop_front() {
        for next in snapshot.data_dependents(&id) {
            if selected.insert(next.clone()) && !progress.owners().contains_key(&next) {
                pending.push_back(next);
            }
        }
    }
    selected.extend(progress.owners().keys().cloned());
    selected
}

pub(super) fn analyze(
    snapshot: &GraphSnapshot,
    progress: &ConsumerProgressSet,
    contracts: &BTreeMap<ComponentId, ComponentRecovery>,
    unconstructed: &BTreeSet<ComponentId>,
) -> Validation {
    match validate_paths(snapshot, progress, contracts, unconstructed) {
        Ok(pending) => Validation {
            pending,
            failure: None,
        },
        Err(error) => Validation::failed(error),
    }
}

fn validate_paths(
    snapshot: &GraphSnapshot,
    progress: &ConsumerProgressSet,
    contracts: &BTreeMap<ComponentId, ComponentRecovery>,
    unconstructed: &BTreeSet<ComponentId>,
) -> GraphResult<BTreeSet<ComponentId>> {
    if progress.graph_id() != snapshot.id.as_ref() {
        return Err(topology("consumer progress belongs to another graph"));
    }
    let producer = snapshot
        .nodes
        .iter()
        .find(|node| node.descriptor.id() == progress.producer_id())
        .ok_or_else(|| topology("consumer progress producer is not declared"))?;
    if !matches!(
        producer.role,
        ComponentRole::Source | ComponentRole::Transformer | ComponentRole::Query
    ) {
        return Err(topology(
            "consumer progress requires a source or transformer producer",
        ));
    }
    if let Some(contract) = contracts.get(progress.producer_id()) {
        if contract.source_admission.is_some()
            || contract.replay_progress.is_some()
            || contract.replay_until.is_some()
        {
            return Err(topology(
                "consumer progress cannot also use admission or a single replay owner",
            ));
        }
    }
    let mut pending = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut reached = BTreeSet::new();
    let mut queue: VecDeque<_> = snapshot
        .data_dependents(progress.producer_id())
        .into_iter()
        .collect();
    while let Some(id) = queue.pop_front() {
        if !visited.insert(id.clone()) {
            return Err(topology(format!(
                "consumer progress has more than one data path to {id}"
            )));
        }
        let Some(node) = snapshot
            .nodes
            .iter()
            .find(|node| node.descriptor.id() == &id)
        else {
            pending.insert(id);
            continue;
        };
        if let Some(expected) = progress.owners().get(&id) {
            reached.insert(id.clone());
            if unconstructed.contains(&id) {
                pending.insert(id);
                continue;
            }
            if let Some(contract) = contracts.get(&id) {
                super::input_progress::validate(contract, node, &snapshot.id).map_err(
                    |source| super::component_error(node, "validate input progress", source),
                )?;
            }
            if !contracts
                .get(&id)
                .and_then(ComponentRecovery::handled_input_progress)
                .is_some_and(|actual| {
                    progress.owns(actual) && std::sync::Arc::ptr_eq(actual, expected)
                })
            {
                return Err(topology(format!(
                    "consumer progress is not bound to {id}'s actual committed-input owner"
                )));
            }
            continue;
        }
        if node.role != ComponentRole::Transformer {
            return Err(topology(format!(
                "consumer progress does not cover required branch {id}"
            )));
        }
        if unconstructed.contains(&id) {
            pending.insert(id.clone());
        } else if !contracts.get(&id).is_some_and(|contract| {
            contract.processing == ProcessingRecovery::Stateless
                && contract.consumer_positions.is_none()
        }) {
            return Err(topology(format!(
                "consumer progress cannot cross {id} without identity-preserving stateless processing"
            )));
        }
        let downstream = snapshot.data_dependents(&id);
        if downstream.is_empty() {
            pending.insert(id);
        } else {
            queue.extend(downstream);
        }
    }
    pending.extend(
        progress
            .owners()
            .keys()
            .filter(|id| !reached.contains(*id))
            .cloned(),
    );
    Ok(pending)
}
