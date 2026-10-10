// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use drasi_core::interface::{FailureSurvival, StorageDurability};

use super::*;
use crate::computation::v1::{
    recovery::ProcessingRecovery, ComponentRecovery, QosChannel, RecoveryGuarantee,
    RecoveryIncompatibility, RecoveryIssue, RecoveryParticipant, RecoveryPathReport,
    RecoveryRequirement, ReplayGapPolicy, RetainedStoreResource, RetentionPolicy,
};

#[derive(Clone, Default)]
struct Coverage {
    until: BTreeSet<ComponentId>,
    handoff: bool,
}

pub(super) fn participants<'a>(
    consumer: &ComponentId,
    connections: impl Iterator<Item = (&'a ComponentId, &'a ComponentId)>,
) -> BTreeSet<ComponentId> {
    let mut incoming: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (from, to) in connections {
        incoming.entry(to).or_default().push(from);
    }
    let mut participants = BTreeSet::new();
    let mut pending = vec![consumer.clone()];
    while let Some(id) = pending.pop() {
        if participants.insert(id.clone()) {
            pending.extend(
                incoming
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .map(|id| (*id).clone()),
            );
        }
    }
    participants
}

fn issue(
    report: &mut RecoveryPathReport,
    participant: RecoveryParticipant,
    guarantee: RecoveryGuarantee,
    reason: RecoveryIncompatibility,
) {
    report.issues.push(RecoveryIssue {
        participant,
        guarantee,
        reason,
    });
}

fn survival(
    report: &mut RecoveryPathReport,
    participant: RecoveryParticipant,
    guarantee: RecoveryGuarantee,
    durability: StorageDurability,
) -> bool {
    let actual = report.requirement.scope.survives(durability);
    if actual == FailureSurvival::Guaranteed {
        true
    } else {
        issue(
            report,
            participant,
            guarantee,
            RecoveryIncompatibility::StorageSurvival { actual },
        );
        false
    }
}

fn transport(
    edge: &EdgeSnapshot,
    resources: &BTreeMap<ResourceId, ResourceHandle>,
) -> Result<Option<StorageDurability>, RecoveryIncompatibility> {
    match &edge.pipe {
        DesiredPipe::Bounded { .. } | DesiredPipe::ByteBounded(_) => Ok(None),
        DesiredPipe::Ranked(config) if !config.drop_when_full => Ok(None),
        DesiredPipe::Ranked(_) | DesiredPipe::Broadcast { .. } => {
            Err(RecoveryIncompatibility::LossyDelivery)
        }
        DesiredPipe::Retained(config) => {
            if config.retention != RetentionPolicy::Backpressure
                || config.gap_policy != ReplayGapPolicy::Strict
            {
                return Err(RecoveryIncompatibility::LossyDelivery);
            }
            let store = resources
                .get(&config.resource)
                .and_then(|resource| resource.get::<RetainedStoreResource>().ok())
                .ok_or(RecoveryIncompatibility::UnknownTransport)?;
            if store.0.capacity() != config.capacity
                || store.0.retention_policy() != config.retention
                || store.0.durable() != config.durable
            {
                return Err(RecoveryIncompatibility::UnknownTransport);
            }
            Ok(Some(store.0.durability()))
        }
        DesiredPipe::Qos(config) => {
            if config.definition.retention != RetentionPolicy::Backpressure
                || config.gap_policy != ReplayGapPolicy::Strict
            {
                return Err(RecoveryIncompatibility::LossyDelivery);
            }
            let channel = resources
                .get(&config.resource)
                .and_then(|resource| resource.get::<QosChannel>().ok())
                .ok_or(RecoveryIncompatibility::UnknownTransport)?;
            if !channel.matches_definition(config) {
                return Err(RecoveryIncompatibility::UnknownTransport);
            }
            Ok(Some(channel.durability()))
        }
        DesiredPipe::External { .. } => Err(RecoveryIncompatibility::UnknownTransport),
    }
}

pub(super) fn assess(
    snapshot: &GraphSnapshot,
    resources: &BTreeMap<ResourceId, ResourceHandle>,
    contracts: &BTreeMap<ComponentId, ComponentRecovery>,
    requirement: &RecoveryRequirement,
) -> RecoveryPathReport {
    let mut report = RecoveryPathReport {
        graph_id: snapshot.id.clone(),
        revision: snapshot.revision,
        requirement: requirement.clone(),
        participants: participants(&requirement.consumer, snapshot.data_connections()),
        issues: Vec::new(),
    };
    let nodes: BTreeMap<_, _> = snapshot
        .nodes
        .iter()
        .map(|node| (node.descriptor.id().clone(), node))
        .collect();
    let mut incoming: BTreeMap<ComponentId, BTreeSet<ComponentId>> = BTreeMap::new();
    let mut outgoing: BTreeMap<ComponentId, BTreeSet<ComponentId>> = BTreeMap::new();
    for (from, to) in snapshot.data_connections() {
        incoming.entry(to.clone()).or_default().insert(from.clone());
        outgoing.entry(from.clone()).or_default().insert(to.clone());
    }
    if requirement.guarantees.is_empty() {
        issue(
            &mut report,
            RecoveryParticipant::Component(requirement.consumer.clone()),
            RecoveryGuarantee::Acceptance,
            RecoveryIncompatibility::EmptyRequirement,
        );
        return report;
    }
    let mut indegree: BTreeMap<_, _> = report
        .participants
        .iter()
        .map(|id| (id.clone(), incoming.get(id).map_or(0, BTreeSet::len)))
        .collect();
    let mut ready: VecDeque<_> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut order = Vec::new();
    while let Some(id) = ready.pop_front() {
        for next in outgoing.get(&id).into_iter().flatten() {
            if let Some(degree) = indegree.get_mut(next) {
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(next.clone());
                }
            }
        }
        order.push(id);
    }
    for guarantee in &requirement.guarantees {
        if order.len() != report.participants.len() {
            issue(
                &mut report,
                RecoveryParticipant::Component(requirement.consumer.clone()),
                *guarantee,
                RecoveryIncompatibility::CyclicPath,
            );
            continue;
        }
        let mut covered: BTreeMap<ComponentId, Coverage> = BTreeMap::new();
        for id in &order {
            let participant = RecoveryParticipant::Component(id.clone());
            let Some(node) = nodes.get(id) else {
                issue(
                    &mut report,
                    participant,
                    *guarantee,
                    RecoveryIncompatibility::UnknownComponent,
                );
                continue;
            };
            let unknown = ComponentRecovery::default();
            let contract = contracts.get(id).unwrap_or(&unknown);
            let predecessors = incoming.get(id);
            if predecessors.map_or(true, BTreeSet::is_empty) {
                let admitted = match contract.admission {
                    Some(durability) => {
                        survival(&mut report, participant.clone(), *guarantee, durability)
                    }
                    None => {
                        issue(
                            &mut report,
                            participant.clone(),
                            *guarantee,
                            RecoveryIncompatibility::UnknownAdmission,
                        );
                        false
                    }
                };
                let mut coverage = Coverage::default();
                if *guarantee == RecoveryGuarantee::Acceptance {
                    covered.insert(id.clone(), coverage);
                    continue;
                }
                if id == &requirement.consumer {
                    issue(
                        &mut report,
                        participant.clone(),
                        *guarantee,
                        RecoveryIncompatibility::NoDataInput,
                    );
                }
                if admitted {
                    if let Some(consumer) = &contract.replay_until {
                        if !report.participants.contains(consumer) || consumer == id {
                            issue(
                                &mut report,
                                participant.clone(),
                                *guarantee,
                                RecoveryIncompatibility::ReplayConsumerOutsidePath,
                            );
                        } else if !matches!(
                            contracts.get(consumer).map(|contract| contract.processing),
                            Some(ProcessingRecovery::Atomic(_))
                        ) {
                            issue(
                                &mut report,
                                participant.clone(),
                                *guarantee,
                                RecoveryIncompatibility::ReplayConsumerWithoutAtomicProgress,
                            );
                        } else {
                            let actual = contracts
                                .get(consumer)
                                .and_then(|owner| owner.committed_progress.as_ref());
                            let same_resource = actual
                                .zip(contract.replay_progress.as_ref())
                                .is_some_and(|(actual, supplied)| {
                                    Arc::ptr_eq(actual, supplied)
                                        && actual.graph_id() == snapshot.id.as_ref()
                                        && actual.component_id() == consumer
                                });
                            if !same_resource {
                                issue(
                                    &mut report,
                                    participant.clone(),
                                    *guarantee,
                                    RecoveryIncompatibility::MismatchedProgressResource,
                                );
                            }
                            let mut paths = BTreeMap::from([(id.clone(), 1usize)]);
                            for upstream in &order {
                                if upstream == consumer {
                                    break;
                                }
                                let count = paths.get(upstream).copied().unwrap_or(0);
                                for (from, to) in snapshot.data_connections() {
                                    if from == upstream {
                                        let next = paths.entry(to.clone()).or_default();
                                        *next = (*next + count).min(2);
                                    }
                                }
                            }
                            if same_resource && paths.get(consumer) == Some(&1) {
                                coverage.until.insert(consumer.clone());
                            } else if paths.get(consumer) != Some(&1) {
                                issue(
                                    &mut report,
                                    participant.clone(),
                                    *guarantee,
                                    RecoveryIncompatibility::AmbiguousReplayPath,
                                );
                            }
                        }
                    }
                    coverage.handoff = contract.publication.is_some_and(|storage| {
                        requirement.scope.survives(storage) == FailureSurvival::Guaranteed
                    });
                }
                covered.insert(id.clone(), coverage);
                continue;
            }
            if *guarantee == RecoveryGuarantee::Acceptance {
                continue;
            }
            let mut inherited: Option<BTreeSet<ComponentId>> = None;
            let mut all_inputs = true;
            for producer in predecessors.into_iter().flatten() {
                let mut previous = covered.get(producer).cloned().unwrap_or_default();
                previous.until.retain(|consumer| {
                    let mut upstream = vec![consumer.clone()];
                    let mut seen = BTreeSet::new();
                    while let Some(candidate) = upstream.pop() {
                        if &candidate == id {
                            return true;
                        }
                        if seen.insert(candidate.clone()) {
                            upstream
                                .extend(incoming.get(&candidate).into_iter().flatten().cloned());
                        }
                    }
                    false
                });
                inherited = Some(match inherited {
                    None => previous.until.clone(),
                    Some(previous_inputs) => previous_inputs
                        .intersection(&previous.until)
                        .cloned()
                        .collect(),
                });
                let edges: Vec<_> = snapshot
                    .edges
                    .iter()
                    .filter(|edge| {
                        &edge.definition.from.component == producer
                            && &edge.definition.to.component == id
                    })
                    .collect();
                let subscription = snapshot
                    .subscriptions
                    .iter()
                    .any(|(from, to)| from == producer && to == id);
                if subscription || edges.is_empty() {
                    issue(
                        &mut report,
                        RecoveryParticipant::Subscription {
                            producer: producer.clone(),
                            consumer: id.clone(),
                        },
                        *guarantee,
                        RecoveryIncompatibility::UnknownSubscription,
                    );
                    all_inputs = false;
                }
                for edge in edges {
                    let edge_participant = RecoveryParticipant::Connection(edge.definition.clone());
                    let stored = match transport(edge, resources) {
                        Ok(stored) => stored,
                        Err(reason) => {
                            issue(&mut report, edge_participant, *guarantee, reason);
                            all_inputs = false;
                            continue;
                        }
                    };
                    let protected = !previous.until.is_empty()
                        || previous.handoff
                            && stored.is_some_and(|storage| {
                                requirement.scope.survives(storage) == FailureSurvival::Guaranteed
                            });
                    if !protected {
                        issue(
                            &mut report,
                            edge_participant,
                            *guarantee,
                            RecoveryIncompatibility::MissingReplayBoundary,
                        );
                        all_inputs = false;
                    }
                }
            }
            let mut continuation = inherited.unwrap_or_default();
            continuation.remove(id);
            let handoff = match contract.processing {
                ProcessingRecovery::Stateless => all_inputs,
                ProcessingRecovery::StatelessWithWakeup => {
                    issue(
                        &mut report,
                        participant.clone(),
                        *guarantee,
                        RecoveryIncompatibility::UntrackedWakeupState,
                    );
                    continuation.clear();
                    false
                }
                ProcessingRecovery::Atomic(storage) => {
                    if !continuation.is_empty() {
                        issue(
                            &mut report,
                            participant.clone(),
                            *guarantee,
                            RecoveryIncompatibility::ReplayConsumerWithoutAtomicProgress,
                        );
                        continuation.clear();
                    }
                    survival(&mut report, participant.clone(), *guarantee, storage)
                        && contract.publication.is_some_and(|storage| {
                            requirement.scope.survives(storage) == FailureSurvival::Guaranteed
                        })
                }
                ProcessingRecovery::Unknown if node.role == ComponentRole::Sink => {
                    if node.completion != Some(SinkCompletion::Handled) {
                        issue(
                            &mut report,
                            participant.clone(),
                            *guarantee,
                            RecoveryIncompatibility::UnhandledAcceptance,
                        );
                    }
                    false
                }
                ProcessingRecovery::Unknown => {
                    issue(
                        &mut report,
                        participant.clone(),
                        *guarantee,
                        RecoveryIncompatibility::UnknownProcessing,
                    );
                    continuation.clear();
                    false
                }
            };
            if id == &requirement.consumer {
                let reason = match guarantee {
                    RecoveryGuarantee::CommittedProcessing
                        if !matches!(contract.processing, ProcessingRecovery::Atomic(_)) =>
                    {
                        Some(RecoveryIncompatibility::NonAtomicProcessing)
                    }
                    RecoveryGuarantee::Publication if contract.publication.is_none() => {
                        Some(RecoveryIncompatibility::MissingPublication)
                    }
                    RecoveryGuarantee::ExternalEffects => {
                        Some(RecoveryIncompatibility::MissingExternalEffectContract)
                    }
                    _ => None,
                };
                if let Some(reason) = reason {
                    issue(&mut report, participant.clone(), *guarantee, reason);
                }
                if *guarantee == RecoveryGuarantee::Publication {
                    for edge in snapshot
                        .unbound_relationships
                        .iter()
                        .filter(|edge| edge.definition.from.component == *id)
                    {
                        issue(
                            &mut report,
                            RecoveryParticipant::Connection(edge.definition.clone()),
                            *guarantee,
                            RecoveryIncompatibility::MissingPublication,
                        );
                    }
                    for edge in snapshot
                        .edges
                        .iter()
                        .filter(|edge| edge.definition.from.component == *id)
                    {
                        let participant = RecoveryParticipant::Connection(edge.definition.clone());
                        match transport(edge, resources) {
                            Ok(Some(storage)) => {
                                survival(&mut report, participant, *guarantee, storage);
                            }
                            Ok(None) => issue(
                                &mut report,
                                participant,
                                *guarantee,
                                RecoveryIncompatibility::MissingPublication,
                            ),
                            Err(reason) => issue(&mut report, participant, *guarantee, reason),
                        }
                    }
                    for (producer, consumer) in snapshot
                        .subscriptions
                        .iter()
                        .filter(|(producer, _)| producer == id)
                    {
                        issue(
                            &mut report,
                            RecoveryParticipant::Subscription {
                                producer: producer.clone(),
                                consumer: consumer.clone(),
                            },
                            *guarantee,
                            RecoveryIncompatibility::UnknownSubscription,
                        );
                    }
                }
            }
            covered.insert(
                id.clone(),
                Coverage {
                    until: continuation,
                    handoff,
                },
            );
        }
    }
    report
}
