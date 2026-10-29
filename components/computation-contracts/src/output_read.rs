// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional, per-output read capabilities.
//!
//! A component registers a [`SnapshotProvider`] and/or a [`ReplayProvider`] for
//! each output port it can actually serve. The two are independent: neither
//! implies the other, and neither implies persistence. Providers derive their
//! answers from the component's own data representation; there is no shared row
//! store, and absence of a provider is explicit rather than an empty result.
//!
//! Every successful answer names the real output epoch ([`GraphProducerIdentity`])
//! and committed logical sequence it observed together, so a consumer can detect
//! an epoch change or a retention gap instead of reading a coherent-looking but
//! different history. Combining a snapshot with a replay from its cursor detects
//! gaps; it is not by itself a gap-free live handover.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

use crate::{
    ChangeEnvelope, ComponentDescriptor, ComponentId, GraphProducerIdentity, GraphProducerProgress,
    PortDirection, PortId, Record, RecordId, RecordImage, SchemaDescriptor, SnapshotFollowProvider,
};

/// One output port of one component.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct OutputEndpoint {
    pub component: ComponentId,
    pub port: PortId,
}

impl OutputEndpoint {
    pub fn new(component: ComponentId, port: PortId) -> Self {
        Self { component, port }
    }
}

/// One declared incoming edge as the consuming component sees it: the input
/// port that receives the data, the producer's actual output port, and the
/// stream that output carries. The graph supplies these from its validated
/// topology; a consumer never derives a port from a stream or chooses a
/// producer's first port.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct InputBinding {
    pub input_port: PortId,
    pub output: OutputEndpoint,
    pub stream: crate::StreamId,
}

impl std::fmt::Display for OutputEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.component, self.port)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutputCapability {
    Snapshot,
    Replay,
    SnapshotFollow,
}

impl std::fmt::Display for OutputCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Snapshot => "snapshot",
            Self::Replay => "replay",
            Self::SnapshotFollow => "snapshot-follow",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OutputReadError {
    /// The component is not currently able to give a coherent answer.
    #[error("output is not ready: {0}")]
    NotReady(String),
    /// Committed history after `requested` is no longer (or not yet) available.
    #[error("output history after {requested} is unavailable; oldest retained {oldest:?}, latest {latest}")]
    Gap {
        requested: u64,
        oldest: Option<u64>,
        latest: u64,
    },
    /// The cursor belongs to another producer epoch than the current output.
    #[error("output epoch changed")]
    EpochChanged {
        requested: Box<GraphProducerIdentity>,
        current: Box<GraphProducerIdentity>,
    },
    /// The provider's component generation or lifecycle ended, or it was replaced.
    #[error("output provider was retired")]
    Retired,
    #[error("output follow lease expired")]
    LeaseExpired,
    /// No such declared output port exists in the current publication.
    #[error("unknown output {0}")]
    UnknownOutput(OutputEndpoint),
    #[error("unknown output component {0}")]
    UnknownComponent(ComponentId),
    /// The output exists but registered no such provider.
    #[error("output {endpoint} does not provide {capability}")]
    Absent {
        endpoint: OutputEndpoint,
        capability: OutputCapability,
    },
    /// A provider's answer violated the output-read contract.
    #[error("invalid output read: {0}")]
    Invalid(String),
    #[error(transparent)]
    Failed(anyhow::Error),
}

impl OutputReadError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

/// A committed logical position within one producer epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCursor {
    producer: GraphProducerIdentity,
    sequence: u64,
}

impl OutputCursor {
    pub fn try_new(
        producer: GraphProducerIdentity,
        sequence: u64,
    ) -> Result<Self, OutputReadError> {
        producer
            .validate()
            .map_err(|error| OutputReadError::invalid(error.to_string()))?;
        Ok(Self { producer, sequence })
    }

    pub fn producer(&self) -> &GraphProducerIdentity {
        &self.producer
    }

    pub fn epoch(&self) -> uuid::Uuid {
        self.producer.incarnation()
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The same epoch at another committed sequence, e.g. 0 for "from the start".
    pub fn at_sequence(&self, sequence: u64) -> Self {
        Self {
            producer: self.producer.clone(),
            sequence,
        }
    }

    /// Reject a cursor from another producer epoch, stream or scope.
    pub fn ensure_same_epoch(&self, current: &Self) -> Result<(), OutputReadError> {
        if self.producer == current.producer {
            Ok(())
        } else {
            Err(OutputReadError::EpochChanged {
                requested: Box::new(self.producer.clone()),
                current: Box::new(current.producer.clone()),
            })
        }
    }
}

/// The complete output contents at one cursor, as ordinary full-image records
/// of the output's declared schema. `dataset` scopes an empty snapshot
/// explicitly; transport size budgets are enforced by the transport, not here.
#[derive(Clone)]
pub struct OutputSnapshot {
    cursor: OutputCursor,
    dataset: String,
    schema: SchemaDescriptor,
    records: im::HashMap<RecordId, Record>,
}

impl OutputSnapshot {
    pub fn try_new(
        cursor: OutputCursor,
        dataset: impl Into<String>,
        schema: SchemaDescriptor,
        records: im::HashMap<RecordId, Record>,
    ) -> Result<Self, OutputReadError> {
        let dataset = dataset.into();
        if dataset.is_empty() {
            return Err(OutputReadError::invalid("empty snapshot dataset scope"));
        }
        for (identity, record) in &records {
            if identity != record.identity() || record.schema() != &schema {
                return Err(OutputReadError::invalid(
                    "snapshot record is keyed by another identity or has another schema",
                ));
            }
            if record.image() != RecordImage::Full {
                return Err(OutputReadError::invalid(
                    "a complete snapshot cannot contain patch or partial records",
                ));
            }
        }
        Ok(Self {
            cursor,
            dataset,
            schema,
            records,
        })
    }

    pub fn cursor(&self) -> &OutputCursor {
        &self.cursor
    }
    pub fn dataset(&self) -> &str {
        &self.dataset
    }
    pub fn schema(&self) -> &SchemaDescriptor {
        &self.schema
    }
    pub fn records(&self) -> &im::HashMap<RecordId, Record> {
        &self.records
    }
}

impl std::fmt::Debug for OutputSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputSnapshot")
            .field("cursor", &self.cursor)
            .field("dataset", &self.dataset)
            .field("records", &self.records.len())
            .finish()
    }
}

/// Committed output after a cursor, through the provider's committed head.
/// Positions are the producer's logical sequences, not transport sequences, so
/// re-emitted history keeps its position.
#[derive(Debug, Clone)]
pub struct OutputReplay {
    after: OutputCursor,
    head: OutputCursor,
    envelopes: Vec<ChangeEnvelope>,
}

impl OutputReplay {
    /// Every envelope must carry the producer's current progress marker for
    /// exactly `head`'s producer and epoch, with logical sequences strictly
    /// increasing within `(after, head]`. The provider remains responsible for
    /// reporting a gap rather than omitting retained history.
    pub fn try_new(
        after: OutputCursor,
        head: OutputCursor,
        envelopes: Vec<ChangeEnvelope>,
    ) -> Result<Self, OutputReadError> {
        after.ensure_same_epoch(&head)?;
        if after.sequence > head.sequence {
            return Err(OutputReadError::invalid(
                "replay cursor is beyond the committed head",
            ));
        }
        let mut previous = after.sequence;
        for envelope in &envelopes {
            let progress = GraphProducerProgress::from_envelope(envelope)
                .map_err(|error| OutputReadError::invalid(error.to_string()))?
                .ok_or_else(|| {
                    OutputReadError::invalid("replayed output lacks its producer's progress marker")
                })?;
            if progress.identity() != &head.producer {
                return Err(OutputReadError::invalid(
                    "replayed output belongs to another producer or epoch",
                ));
            }
            let sequence = progress.sequence();
            if sequence <= previous || sequence > head.sequence {
                return Err(OutputReadError::invalid(
                    "replayed output is out of order or outside the committed range",
                ));
            }
            previous = sequence;
        }
        if previous != head.sequence {
            return Err(OutputReadError::invalid(
                "replay does not reach its reported committed head",
            ));
        }
        Ok(Self {
            after,
            head,
            envelopes,
        })
    }

    pub fn after(&self) -> &OutputCursor {
        &self.after
    }
    pub fn head(&self) -> &OutputCursor {
        &self.head
    }
    pub fn envelopes(&self) -> &[ChangeEnvelope] {
        &self.envelopes
    }
    pub fn into_envelopes(self) -> Vec<ChangeEnvelope> {
        self.envelopes
    }
}

/// Produces a coherent, scoped snapshot from the component's own state. Must
/// return a typed error when it cannot; never an empty success.
#[async_trait]
pub trait SnapshotProvider: Send + Sync {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError>;
}

/// Replays committed output envelopes from retained history. Independent of
/// [`SnapshotProvider`]; retention and persistence are the provider's own.
#[async_trait]
pub trait ReplayProvider: Send + Sync {
    /// The current committed head, usable as `after` for a first replay.
    async fn committed_head(&self) -> Result<OutputCursor, OutputReadError>;

    /// Output after `after`, or [`OutputReadError::EpochChanged`] /
    /// [`OutputReadError::Gap`] when that history cannot be supplied.
    async fn replay_after(&self, after: &OutputCursor) -> Result<OutputReplay, OutputReadError>;
}

/// Providers registered for one output port.
#[derive(Clone, Default)]
pub struct OutputPortReaders {
    snapshot: Option<Arc<dyn SnapshotProvider>>,
    replay: Option<Arc<dyn ReplayProvider>>,
    snapshot_follow: Option<Arc<dyn SnapshotFollowProvider>>,
}

impl OutputPortReaders {
    pub fn snapshot(&self) -> Option<&Arc<dyn SnapshotProvider>> {
        self.snapshot.as_ref()
    }
    pub fn replay(&self) -> Option<&Arc<dyn ReplayProvider>> {
        self.replay.as_ref()
    }
    pub fn snapshot_follow(&self) -> Option<&Arc<dyn SnapshotFollowProvider>> {
        self.snapshot_follow.as_ref()
    }
}

/// A component's registered providers, keyed by output port. Capability is
/// exactly the set of registered providers; there are no separate flags.
#[derive(Clone, Default)]
pub struct OutputReaders {
    ports: BTreeMap<PortId, OutputPortReaders>,
}

impl OutputReaders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_snapshot(mut self, port: PortId, provider: Arc<dyn SnapshotProvider>) -> Self {
        self.ports.entry(port).or_default().snapshot = Some(provider);
        self
    }

    pub fn with_replay(mut self, port: PortId, provider: Arc<dyn ReplayProvider>) -> Self {
        self.ports.entry(port).or_default().replay = Some(provider);
        self
    }

    pub fn with_snapshot_follow(
        mut self,
        port: PortId,
        provider: Arc<dyn SnapshotFollowProvider>,
    ) -> Self {
        self.ports.entry(port).or_default().snapshot_follow = Some(provider);
        self
    }

    pub fn ports(&self) -> &BTreeMap<PortId, OutputPortReaders> {
        &self.ports
    }

    pub fn is_empty(&self) -> bool {
        self.ports.is_empty()
    }

    /// Every registration must name a declared output port.
    pub fn validate(&self, descriptor: &ComponentDescriptor) -> Result<(), OutputReadError> {
        for port in self.ports.keys() {
            let declared = descriptor.ports().iter().any(|declared| {
                declared.id() == port && declared.direction() == PortDirection::Output
            });
            if !declared {
                return Err(OutputReadError::invalid(format!(
                    "{} registers output readers for undeclared output port {port}",
                    descriptor.id()
                )));
            }
        }
        Ok(())
    }

    /// Non-owning copy for registry publication.
    pub fn downgrade(&self) -> WeakOutputReaders {
        WeakOutputReaders {
            ports: self
                .ports
                .iter()
                .map(|(port, readers)| {
                    (
                        port.clone(),
                        WeakOutputPortReaders {
                            snapshot: readers.snapshot.as_ref().map(Arc::downgrade),
                            replay: readers.replay.as_ref().map(Arc::downgrade),
                            snapshot_follow: readers.snapshot_follow.as_ref().map(Arc::downgrade),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
pub struct WeakOutputPortReaders {
    snapshot: Option<std::sync::Weak<dyn SnapshotProvider>>,
    replay: Option<std::sync::Weak<dyn ReplayProvider>>,
    snapshot_follow: Option<std::sync::Weak<dyn SnapshotFollowProvider>>,
}

impl WeakOutputPortReaders {
    pub fn snapshot_is_provided(&self) -> bool {
        self.snapshot.is_some()
    }
    pub fn replay_is_provided(&self) -> bool {
        self.replay.is_some()
    }
    pub fn snapshot_follow_is_provided(&self) -> bool {
        self.snapshot_follow.is_some()
    }

    pub fn upgrade_snapshot_follow(
        &self,
    ) -> Result<Option<Arc<dyn SnapshotFollowProvider>>, OutputReadError> {
        self.snapshot_follow
            .as_ref()
            .map(|provider| provider.upgrade().ok_or(OutputReadError::Retired))
            .transpose()
    }

    pub fn upgrade_snapshot(&self) -> Result<Option<Arc<dyn SnapshotProvider>>, OutputReadError> {
        self.snapshot
            .as_ref()
            .map(|provider| provider.upgrade().ok_or(OutputReadError::Retired))
            .transpose()
    }

    pub fn upgrade_replay(&self) -> Result<Option<Arc<dyn ReplayProvider>>, OutputReadError> {
        self.replay
            .as_ref()
            .map(|provider| provider.upgrade().ok_or(OutputReadError::Retired))
            .transpose()
    }

    /// Whether both publications name the same registered provider objects.
    pub fn same_providers(&self, other: &Self) -> bool {
        fn same<T: ?Sized>(
            left: &Option<std::sync::Weak<T>>,
            right: &Option<std::sync::Weak<T>>,
        ) -> bool {
            match (left, right) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    left.as_ptr() as *const () == right.as_ptr() as *const ()
                }
                _ => false,
            }
        }
        same(&self.snapshot, &other.snapshot)
            && same(&self.replay, &other.replay)
            && same(&self.snapshot_follow, &other.snapshot_follow)
    }
}

/// Weak publication of [`OutputReaders`], so a graph registry never keeps a
/// retired component's provider alive.
#[derive(Clone, Default)]
pub struct WeakOutputReaders {
    ports: BTreeMap<PortId, WeakOutputPortReaders>,
}

impl WeakOutputReaders {
    pub fn port(&self, port: &PortId) -> Option<&WeakOutputPortReaders> {
        self.ports.get(port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamId;

    fn producer(epoch: u128) -> GraphProducerIdentity {
        GraphProducerIdentity::for_epoch(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("component").unwrap(),
            StreamId::try_new("component/out").unwrap(),
            uuid::Uuid::from_u128(epoch),
            true,
        )
        .unwrap()
    }

    #[test]
    fn cursors_of_different_epochs_are_never_comparable() {
        let first = OutputCursor::try_new(producer(1), 4).unwrap();
        let second = OutputCursor::try_new(producer(2), 4).unwrap();
        assert!(first.ensure_same_epoch(&first.at_sequence(0)).is_ok());
        assert!(matches!(
            first.ensure_same_epoch(&second),
            Err(OutputReadError::EpochChanged { .. })
        ));
        assert!(matches!(
            OutputReplay::try_new(first, second, Vec::new()),
            Err(OutputReadError::EpochChanged { .. })
        ));
    }

    #[test]
    fn replay_rejects_a_cursor_beyond_its_head() {
        let ahead = OutputCursor::try_new(producer(1), 5).unwrap();
        let head = ahead.at_sequence(4);
        assert!(matches!(
            OutputReplay::try_new(ahead, head, Vec::new()),
            Err(OutputReadError::Invalid(_))
        ));
    }

    #[test]
    fn replay_cannot_report_a_newer_head_without_returning_its_history() {
        let after = OutputCursor::try_new(producer(1), 0).unwrap();
        let head = after.at_sequence(1);
        assert!(matches!(
            OutputReplay::try_new(after.clone(), head, Vec::new()),
            Err(OutputReadError::Invalid(_))
        ));
        assert!(OutputReplay::try_new(after.clone(), after, Vec::new()).is_ok());
    }

    #[test]
    fn readers_must_name_declared_output_ports() {
        struct Never;
        #[async_trait]
        impl SnapshotProvider for Never {
            async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
                Err(OutputReadError::NotReady("test".into()))
            }
        }
        let descriptor =
            ComponentDescriptor::try_new(ComponentId::try_new("component").unwrap(), vec![])
                .unwrap();
        let readers =
            OutputReaders::new().with_snapshot(PortId::try_new("out").unwrap(), Arc::new(Never));
        assert!(readers.validate(&descriptor).is_err());
        assert!(OutputReaders::new().validate(&descriptor).is_ok());
    }

    #[test]
    fn weak_publication_does_not_retain_the_provider() {
        struct Never;
        #[async_trait]
        impl SnapshotProvider for Never {
            async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
                Err(OutputReadError::NotReady("test".into()))
            }
        }
        let port = PortId::try_new("out").unwrap();
        let provider: Arc<dyn SnapshotProvider> = Arc::new(Never);
        let readers = OutputReaders::new().with_snapshot(port.clone(), provider.clone());
        let weak = readers.downgrade();
        let published = weak.port(&port).unwrap();
        assert!(published.snapshot_is_provided() && !published.replay_is_provided());
        assert!(published.upgrade_snapshot().unwrap().is_some());
        drop(readers);
        drop(provider);
        assert!(matches!(
            published.upgrade_snapshot(),
            Err(OutputReadError::Retired)
        ));
    }

    struct Permissive;

    impl crate::RecordValidator for Permissive {
        fn validate_identity(
            &self,
            _: &SchemaDescriptor,
            _: &RecordId,
        ) -> std::result::Result<(), crate::RecordValidationError> {
            Ok(())
        }

        fn validate(
            &self,
            _: &SchemaDescriptor,
            _: &RecordId,
            _: RecordImage,
            _: &[u8],
        ) -> std::result::Result<(), crate::RecordValidationError> {
            Ok(())
        }
    }

    fn snapshot_with(image: RecordImage, dataset: &str) -> Result<OutputSnapshot, OutputReadError> {
        let schema = crate::Schema::new(
            SchemaDescriptor::try_new(
                crate::SchemaId::try_new("test.records").unwrap(),
                crate::SchemaVersion::try_new(1).unwrap(),
                "none",
                bytes::Bytes::from_static(b"records"),
            )
            .unwrap(),
            Arc::new(Permissive),
        );
        let identity = RecordId::try_new("ns", bytes::Bytes::from_static(b"a")).unwrap();
        let record = Record::try_new(
            &schema,
            identity.clone(),
            image,
            bytes::Bytes::from_static(b"payload"),
        )
        .unwrap();
        OutputSnapshot::try_new(
            OutputCursor::try_new(producer(1), 1).unwrap(),
            dataset,
            schema.descriptor().clone(),
            im::HashMap::unit(identity, record),
        )
    }

    #[test]
    fn a_complete_snapshot_accepts_only_full_images_and_a_nonempty_scope() {
        assert!(snapshot_with(RecordImage::Full, "dataset").is_ok());
        for image in [RecordImage::Patch, RecordImage::Partial] {
            assert!(matches!(
                snapshot_with(image, "dataset"),
                Err(OutputReadError::Invalid(_))
            ));
        }
        assert!(matches!(
            snapshot_with(RecordImage::Full, ""),
            Err(OutputReadError::Invalid(_))
        ));
        // Transport budgets, not this contract, bound the scope's length.
        assert!(snapshot_with(RecordImage::Full, &"d".repeat(4096)).is_ok());
    }

    fn envelope(
        identity: Option<&GraphProducerIdentity>,
        logical: u64,
        transport: u64,
    ) -> ChangeEnvelope {
        use drasi_core_types::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };
        let stream = producer(1).stream().clone();
        let mut envelope = crate::GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("s", "a"),
                        labels: Arc::from(Vec::<Arc<str>>::new()),
                        effective_from: 1,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({})),
                },
            },
            stream,
            transport,
            None,
        )
        .unwrap();
        if let Some(identity) = identity {
            GraphProducerProgress::annotate(&mut envelope, identity, logical).unwrap();
        }
        envelope
    }

    #[test]
    fn replay_uses_the_logical_progress_marker_and_survives_reemission() {
        let after = OutputCursor::try_new(producer(1), 1).unwrap();
        let head = after.at_sequence(3);
        let identity = producer(1);
        let second = envelope(Some(&identity), 2, 2);
        let third = envelope(Some(&identity), 3, 3);
        assert!(OutputReplay::try_new(
            after.clone(),
            head.clone(),
            vec![second.clone(), third.clone()]
        )
        .is_ok());
        // Transport sequences are unrelated to the committed cursor.
        let reemitted = vec![second.reemit(900).unwrap(), third.reemit(1).unwrap()];
        assert_eq!(reemitted[0].system().sequence(), 900);
        let replay = OutputReplay::try_new(after.clone(), head.clone(), reemitted).unwrap();
        assert_eq!(replay.envelopes().len(), 2);

        // Logical order and range are enforced irrespective of transport order.
        assert!(matches!(
            OutputReplay::try_new(
                after.clone(),
                head.clone(),
                vec![third.clone(), second.clone()]
            ),
            Err(OutputReadError::Invalid(_))
        ));
        assert!(matches!(
            OutputReplay::try_new(after.clone(), after.clone(), vec![second.clone()]),
            Err(OutputReadError::Invalid(_))
        ));
        let stale = envelope(Some(&identity), 1, 50);
        assert!(matches!(
            OutputReplay::try_new(after.clone(), head.clone(), vec![stale]),
            Err(OutputReadError::Invalid(_))
        ));
    }

    #[test]
    fn replay_rejects_missing_or_foreign_producer_markers() {
        let after = OutputCursor::try_new(producer(1), 0).unwrap();
        let head = after.at_sequence(2);
        for forged in [envelope(None, 1, 1), envelope(Some(&producer(2)), 1, 1)] {
            assert!(
                matches!(
                    OutputReplay::try_new(after.clone(), head.clone(), vec![forged]),
                    Err(OutputReadError::Invalid(_))
                ),
                "an envelope not owned by the head's epoch must be rejected"
            );
        }
    }
}
