// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc};

use crate::computation::v1::{
    producer_progress::{GraphInputProgress, QueryInputOwner},
    recovery::ProcessingRecovery,
    ChangeEnvelope, ComponentRecovery, ComponentRole, GraphProducerIdentity, GraphProducerProgress,
    QueryChangeCodec, QuerySourceProgress, SinkCompletion, SourceProgressKey,
    SourceProgressSnapshot,
};
use drasi_core::interface::SourceCheckpoint;

use super::NodeSnapshot;

const MAX_HANDLED_INPUT_KEYS: usize = 4096;

pub(super) fn validate(
    contract: &ComponentRecovery,
    node: &NodeSnapshot,
    graph_id: &str,
) -> anyhow::Result<()> {
    let Some(progress) = &contract.volatile_input_progress else {
        return Ok(());
    };
    anyhow::ensure!(
        node.role == ComponentRole::Transformer
            || (node.role == ComponentRole::Sink
                && node.completion == Some(SinkCompletion::Handled)),
        "volatile input progress requires a transformer or Handled sink"
    );
    anyhow::ensure!(
        contract.committed_progress.is_none()
            && !matches!(contract.processing, ProcessingRecovery::Atomic(_)),
        "volatile input progress cannot replace component-owned atomic progress"
    );
    anyhow::ensure!(
        progress.graph_id() == graph_id && progress.component_id() == node.descriptor.id(),
        "volatile input progress belongs to another graph or component"
    );
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
enum InputOwner {
    Graph(GraphProducerIdentity),
    Query(QueryInputOwner),
}

pub(super) struct PreparedInput {
    progress: GraphInputProgress,
    owner: Option<InputOwner>,
    pub(super) duplicate: bool,
}

/// Lives on the actual instance, including across processing pauses and stops.
pub(super) struct InputProgress {
    progress: Arc<QuerySourceProgress>,
    identity: Arc<()>,
    claimed: bool,
    owners: BTreeMap<SourceProgressKey, InputOwner>,
}

impl InputProgress {
    pub(super) fn configure(
        current: &mut Option<Box<Self>>,
        requested: Option<&Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<()> {
        if current
            .as_ref()
            .zip(requested)
            .is_some_and(|(current, requested)| Arc::ptr_eq(&current.progress, requested))
        {
            return Ok(());
        }
        anyhow::ensure!(
            !current.as_ref().is_some_and(|current| current.claimed),
            "changing volatile input progress requires a successful stop"
        );
        *current = requested.map(|progress| {
            Box::new(Self {
                progress: progress.clone(),
                identity: Arc::new(()),
                claimed: false,
                owners: BTreeMap::new(),
            })
        });
        Ok(())
    }

    pub(super) fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.claimed,
            "volatile input progress requires cleanup before restart"
        );
        self.progress.claim_volatile_writer(&self.identity)?;
        self.claimed = true;
        Ok(())
    }

    pub(super) fn ready(&self) {
        let mut snapshot = self.progress.snapshot().as_ref().clone();
        snapshot.ready = true;
        snapshot.admitting = true;
        snapshot.recovered = true;
        snapshot.bootstrap_complete = true;
        snapshot.persistent = false;
        snapshot.failure = None;
        self.progress.publish(snapshot);
    }

    pub(super) fn stopped(&mut self) -> anyhow::Result<()> {
        if self.claimed {
            self.progress.release_volatile_writer(&self.identity)?;
            self.claimed = false;
        }
        Ok(())
    }

    pub(super) fn fence(&self) -> Fence {
        Fence(self.claimed.then(|| self.progress.clone()))
    }

    fn prepare_with(
        &self,
        input: &ChangeEnvelope,
        producer_role: ComponentRole,
        snapshot: &SourceProgressSnapshot,
        owners: &BTreeMap<SourceProgressKey, InputOwner>,
    ) -> anyhow::Result<PreparedInput> {
        anyhow::ensure!(
            self.claimed && snapshot.ready && snapshot.admitting && snapshot.failure.is_none(),
            "volatile input progress is not active"
        );
        let (progress, query_owner) = GraphInputProgress::from_processing_input(
            input,
            Some(&self.progress),
            Some(producer_role),
        )?;
        let progress = progress
            .with_consumer_boundary(input, Some(&self.progress))?
            .with_transport(input);
        anyhow::ensure!(
            progress.position.as_ref().map_or(true, |position| {
                position.len() <= drasi_computation_contracts::MAX_SOURCE_POSITION_BYTES
            }),
            "volatile input position exceeds the source cursor limit"
        );
        let owner = if let Some(owner) = query_owner {
            Some(InputOwner::Query(owner))
        } else {
            let producer = if QueryChangeCodec::is_query_output(input) {
                GraphProducerProgress::from_query_envelope(input)?
            } else {
                GraphProducerProgress::from_envelope(input)?
            };
            progress
                .input_producer()
                .cloned()
                .or_else(|| {
                    producer
                        .filter(|producer| {
                            progress.identity
                                == SourceProgressKey::Stream(producer.identity().stream().clone())
                        })
                        .map(|producer| producer.identity().clone())
                })
                .map(InputOwner::Graph)
        };
        let saved = snapshot.checkpoints.get(&progress.identity);
        match (owners.get(&progress.identity), &owner) {
            (Some(previous), Some(owner)) => {
                anyhow::ensure!(previous == owner, "handled input producer identity changed");
            }
            (Some(_), None) => anyhow::bail!("input omitted its handled producer identity"),
            (None, Some(_)) => {
                anyhow::ensure!(
                    saved.is_none(),
                    "handled input has no verified producer identity"
                );
            }
            (None, None) => {}
        }
        let duplicate = self
            .progress
            .position_handled(&progress, saved)?
            .unwrap_or_else(|| saved.is_some_and(|saved| saved.sequence >= progress.sequence));
        if !duplicate && owner.is_some() {
            anyhow::ensure!(
                saved.map_or(Some(1), |saved| saved.sequence.checked_add(1))
                    == Some(progress.sequence),
                "handled input logical progress has a gap"
            );
        }
        let prepared = PreparedInput {
            progress,
            owner,
            duplicate,
        };
        let mut transports = snapshot.transport_sequences.clone();
        prepared.progress.record_transport(&mut transports);
        anyhow::ensure!(
            snapshot.checkpoints.len()
                + usize::from(
                    !snapshot
                        .checkpoints
                        .contains_key(&prepared.progress.identity)
                )
                + transports.len()
                <= MAX_HANDLED_INPUT_KEYS,
            "volatile handled input progress limit reached"
        );
        Ok(prepared)
    }

    pub(super) fn prepare(
        &self,
        input: &ChangeEnvelope,
        producer_role: ComponentRole,
    ) -> anyhow::Result<Box<PreparedInput>> {
        self.prepare_with(
            input,
            producer_role,
            &self.progress.snapshot(),
            &self.owners,
        )
        .map(Box::new)
    }

    pub(super) fn prepare_batch<'a>(
        &self,
        inputs: impl Iterator<Item = (&'a ChangeEnvelope, ComponentRole)>,
    ) -> anyhow::Result<Vec<PreparedInput>> {
        let mut snapshot = self.progress.snapshot().as_ref().clone();
        let mut owners = self.owners.clone();
        inputs
            .map(|(input, producer_role)| {
                let prepared = self.prepare_with(input, producer_role, &snapshot, &owners)?;
                Self::record(&prepared, &mut snapshot, &mut owners);
                Ok(prepared)
            })
            .collect()
    }

    fn record(
        input: &PreparedInput,
        snapshot: &mut SourceProgressSnapshot,
        owners: &mut BTreeMap<SourceProgressKey, InputOwner>,
    ) {
        input
            .progress
            .record_transport(&mut snapshot.transport_sequences);
        if !input.duplicate {
            snapshot.checkpoints.insert(
                input.progress.identity.clone(),
                SourceCheckpoint::new(input.progress.sequence, input.progress.position.clone()),
            );
            if let Some(owner) = &input.owner {
                owners.insert(input.progress.identity.clone(), owner.clone());
            }
        }
    }

    pub(super) fn confirm(&mut self, inputs: impl IntoIterator<Item = PreparedInput>) {
        let mut snapshot = self.progress.snapshot().as_ref().clone();
        for input in inputs {
            Self::record(&input, &mut snapshot, &mut self.owners);
        }
        self.progress.publish(snapshot);
    }
}

impl Drop for InputProgress {
    fn drop(&mut self) {
        if self.claimed {
            self.progress.fail();
        }
    }
}

pub(super) struct Fence(Option<Arc<QuerySourceProgress>>);

impl Fence {
    pub(super) fn completed(mut self) {
        self.0 = None;
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        if let Some(progress) = &self.0 {
            progress.fail();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::{
        ComponentId, GraphChangeCodec, InputPositionComparator, InputPositionPolicy, StreamId,
        SystemMetadata,
    };
    use bytes::Bytes;

    fn owner() -> Arc<QuerySourceProgress> {
        Arc::new(
            QuerySourceProgress::new("test", ComponentId::try_new("sink").expect("sink"))
                .expect("owner"),
        )
    }

    fn tracker(owner: &Arc<QuerySourceProgress>) -> Box<InputProgress> {
        let mut tracker = None;
        InputProgress::configure(&mut tracker, Some(owner)).expect("configure");
        tracker.expect("tracker")
    }

    fn input(sequence: u64, position: Option<&[u8]>) -> ChangeEnvelope {
        let stream = StreamId::try_new("source/out").expect("stream");
        let envelope = GraphChangeCodec::encode_changes(&[], stream.clone(), sequence, None)
            .expect("envelope");
        let mut system = SystemMetadata::new(stream, sequence);
        if let Some(position) = position {
            system = system.with_source_position(Bytes::copy_from_slice(position));
        }
        ChangeEnvelope::new(envelope.id().clone(), envelope.changes().clone(), system)
    }

    #[test]
    fn volatile_publisher_ownership_requires_cleanup_and_replacement_resets_the_generation() {
        let owner = owner();
        let mut first = tracker(&owner);
        first.start().expect("claim");
        first.ready();
        let prepared = first
            .prepare(&input(1, None), ComponentRole::Source)
            .expect("prepare");
        first.confirm([*prepared]);
        let generation = owner.snapshot().reset_generation;
        let mut replacement = tracker(&owner);
        assert!(replacement.start().is_err());
        assert!(
            owner.snapshot().ready,
            "rejected claims cannot fence another owner"
        );
        drop(first.fence());
        assert!(owner.snapshot().failure.is_some());
        assert!(
            first.start().is_err(),
            "failed work requires successful stop"
        );
        first.stopped().expect("cleanup");
        first.start().expect("same instance restart");
        first.ready();
        assert!(
            first
                .prepare(&input(1, None), ComponentRole::Source)
                .expect("old input")
                .duplicate
        );
        assert_eq!(owner.snapshot().reset_generation, generation);
        first.stopped().expect("stop old instance");
        replacement.start().expect("replace stopped instance");
        replacement.ready();
        assert_eq!(owner.snapshot().reset_generation, generation + 1);
        assert!(owner.snapshot().checkpoints.is_empty());
        assert!(first.start().is_err());
        drop(replacement);
        assert!(owner.snapshot().failure.is_some());
        assert!(
            tracker(&owner).start().is_err(),
            "dropping is not successful cleanup"
        );
    }

    struct Positions;
    impl InputPositionComparator for Positions {
        fn is_after(&self, event: &[u8], handled: &[u8]) -> anyhow::Result<bool> {
            anyhow::ensure!(
                event.len() == 1 && handled.len() == 1,
                "malformed test position"
            );
            Ok(event[0] > handled[0])
        }
    }

    #[test]
    fn batch_preflight_does_not_publish_and_duplicates_only_advance_transport() {
        let owner = owner();
        let stream = StreamId::try_new("source/out").expect("stream");
        owner
            .bind_input_boundary(
                &ComponentId::try_new("source").expect("source"),
                ComponentRole::Source,
                Some(Arc::new(
                    InputPositionPolicy::new(
                        SourceProgressKey::Stream(stream.clone()),
                        Arc::new(Positions),
                    )
                    .expect("position policy"),
                )),
                vec![stream.clone()],
            )
            .expect("validated boundary");
        let mut tracker = tracker(&owner);
        tracker.start().expect("start");
        tracker.ready();
        let inputs = [input(1, Some(&[10])), input(2, Some(&[10])), input(3, Some(&[20]))];
        let batch = tracker
            .prepare_batch(inputs.iter().map(|input| (input, ComponentRole::Source)))
            .expect("batch");
        assert_eq!(
            batch
                .iter()
                .map(|input| input.duplicate)
                .collect::<Vec<_>>(),
            [false, true, false]
        );
        assert!(owner.snapshot().checkpoints.is_empty());
        tracker.confirm(batch);
        let key = SourceProgressKey::Stream(stream.clone());
        let checkpoint = owner.snapshot().checkpoints[&key].clone();
        let old = tracker
            .prepare(&input(4, Some(&[10])), ComponentRole::Source)
            .expect("old physical position");
        assert!(old.duplicate);
        tracker.confirm([*old]);
        assert_eq!(owner.snapshot().checkpoints[&key], checkpoint);
        assert_eq!(owner.snapshot().transport_sequences[&stream], 4);
        assert!(
            tracker
                .prepare(&input(2, Some(&[30])), ComponentRole::Source)
                .is_err(),
            "new positions cannot regress counters"
        );
        assert!(tracker
            .prepare(&input(5, None), ComponentRole::Source)
            .is_err());
        assert!(tracker
            .prepare(&input(5, Some(&[1, 2])), ComponentRole::Source)
            .is_err());
        assert_eq!(owner.snapshot().checkpoints[&key], checkpoint);
        tracker.stopped().expect("stop");
    }
}
