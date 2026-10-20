// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{ComponentId, QuerySourceProgress, SourceProgressSnapshot};
use async_trait::async_trait;
use futures::{stream::FuturesUnordered, StreamExt};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::watch;

pub use drasi_computation_contracts::{
    ConsumerProgressProvider, ConsumerProgressSnapshot, ConsumerProgressUpdates,
    ConsumerProgressView, InputPositionComparator, InputPositionPolicy,
    MAX_CONSUMER_PROGRESS_MEMBERS,
};

/// Actual local progress owners for one producer's required consumers.
///
/// This retains each obligation independently. Read-only plugin views cannot
/// replace the owners, and equal graph/component names are not ownership proof.
/// Readiness is an observation, not a lease permitting concurrent reset or
/// membership changes; the graph must coordinate those lifecycle boundaries.
#[derive(Debug)]
pub struct ConsumerProgressSet {
    graph_id: Arc<str>,
    producer_id: ComponentId,
    owners: BTreeMap<ComponentId, Arc<QuerySourceProgress>>,
}

impl ConsumerProgressSet {
    pub fn new(
        graph_id: impl Into<Arc<str>>,
        producer_id: ComponentId,
        owners: impl IntoIterator<Item = Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<Self> {
        let graph_id = graph_id.into();
        super::data::validate_identifier("graph", &graph_id)?;
        let mut members = BTreeMap::new();
        for owner in owners {
            anyhow::ensure!(
                members.len() < MAX_CONSUMER_PROGRESS_MEMBERS,
                "consumer progress requires at most {MAX_CONSUMER_PROGRESS_MEMBERS} owners"
            );
            anyhow::ensure!(
                owner.graph_id() == graph_id.as_ref() && owner.component_id() != &producer_id,
                "consumer progress must belong to another component in the producer's graph"
            );
            anyhow::ensure!(
                members
                    .insert(owner.component_id().clone(), owner)
                    .is_none(),
                "duplicate consumer progress owner"
            );
        }
        anyhow::ensure!(
            !members.is_empty(),
            "consumer progress requires at least one owner"
        );
        Ok(Self {
            graph_id,
            producer_id,
            owners: members,
        })
    }

    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }

    pub fn producer_id(&self) -> &ComponentId {
        &self.producer_id
    }

    pub fn owners(&self) -> &BTreeMap<ComponentId, Arc<QuerySourceProgress>> {
        &self.owners
    }

    pub fn owns(&self, owner: &Arc<QuerySourceProgress>) -> bool {
        self.owners
            .get(owner.component_id())
            .is_some_and(|actual| Arc::ptr_eq(actual, owner))
    }

    pub fn read_only(self: &Arc<Self>) -> ConsumerProgressView {
        ConsumerProgressView::new(self.clone())
    }
}

impl ConsumerProgressProvider for ConsumerProgressSet {
    fn graph_id(&self) -> &str {
        &self.graph_id
    }

    fn producer_id(&self) -> &ComponentId {
        &self.producer_id
    }

    fn snapshot(&self) -> anyhow::Result<ConsumerProgressSnapshot> {
        Ok(ConsumerProgressSnapshot {
            consumers: self
                .owners
                .iter()
                .map(|(id, owner)| (id.clone(), owner.snapshot()))
                .collect(),
        })
    }

    fn subscribe(&self) -> anyhow::Result<Box<dyn ConsumerProgressUpdates>> {
        Ok(Box::new(Subscription {
            receivers: self
                .owners
                .iter()
                .map(|(id, owner)| (id.clone(), owner.subscribe()))
                .collect(),
            _owners: self.owners.clone(),
        }))
    }
}

struct Subscription {
    receivers: BTreeMap<ComponentId, watch::Receiver<Arc<SourceProgressSnapshot>>>,
    _owners: BTreeMap<ComponentId, Arc<QuerySourceProgress>>,
}

#[async_trait]
impl ConsumerProgressUpdates for Subscription {
    fn snapshot(&mut self) -> anyhow::Result<ConsumerProgressSnapshot> {
        Ok(ConsumerProgressSnapshot {
            consumers: self
                .receivers
                .iter_mut()
                .map(|(id, receiver)| (id.clone(), receiver.borrow_and_update().clone()))
                .collect(),
        })
    }

    async fn changed(&mut self) -> anyhow::Result<()> {
        let mut waits: FuturesUnordered<_> = self
            .receivers
            .iter()
            .map(|(id, receiver)| {
                let id = id.clone();
                let mut receiver = receiver.clone();
                async move {
                    receiver.changed().await.map_err(|error| {
                        anyhow::anyhow!("consumer {id} progress observation closed: {error}")
                    })
                }
            })
            .collect();
        waits
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("consumer progress subscription has no members"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use drasi_core::interface::SourceCheckpoint;
    use futures::poll;

    fn owner(graph: &str, name: &str) -> Arc<QuerySourceProgress> {
        Arc::new(QuerySourceProgress::new(graph, ComponentId::try_new(name).unwrap()).unwrap())
    }

    fn group(owners: Vec<Arc<QuerySourceProgress>>) -> Arc<ConsumerProgressSet> {
        Arc::new(
            ConsumerProgressSet::new("graph", ComponentId::try_new("producer").unwrap(), owners)
                .unwrap(),
        )
    }

    fn ready(owner: &QuerySourceProgress, persistent: bool, sequence: u64, cursor: &[u8]) {
        owner.publish(SourceProgressSnapshot {
            ready: true,
            recovered: true,
            persistent,
            checkpoints: BTreeMap::from([(
                super::super::SourceProgressKey::Source("input".into()),
                SourceCheckpoint::new(sequence, Some(Bytes::copy_from_slice(cursor))),
            )]),
            ..Default::default()
        });
    }

    #[test]
    fn actual_owners_are_retained_without_synthesizing_a_single_checkpoint() {
        let left = owner("graph", "left");
        let right = owner("graph", "right");
        let group = group(vec![left.clone(), right.clone()]);
        assert!(group.owns(&left));
        assert!(!group.owns(&owner("graph", "left")));
        ready(&left, false, 100, b"partitioned-cursor");
        ready(&right, true, 7, b"opaque-row-cursor");
        let snapshot = group.snapshot().unwrap();
        assert_eq!(snapshot.consumers.len(), 2);
        assert!(snapshot.ready());
        assert!(Arc::ptr_eq(
            &snapshot.consumers[left.component_id()],
            &left.snapshot()
        ));
        assert!(Arc::ptr_eq(
            &snapshot.consumers[right.component_id()],
            &right.snapshot()
        ));
        assert!(!snapshot.consumers[left.component_id()].persistent);
        assert!(snapshot.consumers[right.component_id()].persistent);
        assert!(group.read_only().same_binding(&group.read_only()));
        let replacement = super::ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("producer").unwrap(),
            [left, right],
        )
        .unwrap();
        assert!(!group
            .read_only()
            .same_binding(&Arc::new(replacement).read_only()));
    }

    #[test]
    fn invalid_membership_never_becomes_partial_membership() {
        let producer = ComponentId::try_new("producer").unwrap();
        for owners in [
            vec![],
            vec![owner("other", "left")],
            vec![owner("graph", "producer")],
            vec![owner("graph", "left"), owner("graph", "left")],
            (0..=MAX_CONSUMER_PROGRESS_MEMBERS)
                .map(|index| owner("graph", &format!("consumer-{index}")))
                .collect(),
        ] {
            assert!(ConsumerProgressSet::new("graph", producer.clone(), owners).is_err());
        }
        let largest = ConsumerProgressSet::new(
            "graph",
            producer,
            (0..MAX_CONSUMER_PROGRESS_MEMBERS)
                .map(|index| owner("graph", &format!("consumer-{index}"))),
        )
        .unwrap();
        assert_eq!(largest.owners().len(), MAX_CONSUMER_PROGRESS_MEMBERS);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn readiness_waits_for_every_owner_and_surfaces_the_failed_member() {
        let left = owner("graph", "left");
        let right = owner("graph", "right");
        let view = group(vec![left.clone(), right.clone()]).read_only();
        ready(&left, false, 1, b"left");
        let mut pending = Box::pin(view.wait_ready());
        assert!(poll!(pending.as_mut()).is_pending());
        ready(&right, false, 1, b"right");
        assert!(pending.await.unwrap().ready());
        right.pending();
        assert!(!view.snapshot().unwrap().ready());
        assert_eq!(view.snapshot().unwrap().consumers.len(), 2);
        right.fail();
        let error = view.wait_recovered().await.unwrap_err();
        assert!(error.to_string().contains("right"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_waits_and_independent_subscriptions_keep_unobserved_changes() {
        let left = owner("graph", "left");
        let right = owner("graph", "right");
        let group = group(vec![left.clone(), right.clone()]);
        let mut first = group.subscribe().unwrap();
        let mut second = group.subscribe().unwrap();
        first.snapshot().unwrap();
        second.snapshot().unwrap();
        let mut waiting = Box::pin(first.changed());
        assert!(poll!(waiting.as_mut()).is_pending());
        drop(waiting);
        ready(&right, false, 2, b"right");
        first.changed().await.unwrap();
        first.changed().await.unwrap();
        assert!(!first.snapshot().unwrap().ready());
        second.changed().await.unwrap();
        assert!(!second.snapshot().unwrap().ready());
        let mut waiting = Box::pin(first.changed());
        assert!(poll!(waiting.as_mut()).is_pending());
        ready(&left, false, 3, b"left");
        waiting.await.unwrap();
        assert!(first.snapshot().unwrap().ready());
        assert!(second.snapshot().unwrap().ready());
    }
}
