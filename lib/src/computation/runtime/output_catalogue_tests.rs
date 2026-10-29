// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Generic output catalogue tests. Query outputs and independently written
//! source/transformer fixtures are read through the same `OutputHandle` path.

use super::*;
use drasi_core_types::data_value::DataValue;
use std::collections::{BTreeMap as Map, VecDeque};
use std::num::NonZeroUsize;

type ReadResult<T> = std::result::Result<T, OutputReadError>;

struct MarkInputFactory;
struct MarkInput;

impl drasi_core::interface::SourceMiddlewareFactory for MarkInputFactory {
    fn name(&self) -> String {
        "mark-input".into()
    }
    fn create(
        &self,
        _: &drasi_core::models::SourceMiddlewareConfig,
    ) -> std::result::Result<
        Arc<dyn drasi_core::interface::SourceMiddleware>,
        drasi_core::interface::MiddlewareSetupError,
    > {
        Ok(Arc::new(MarkInput))
    }
}

#[async_trait]
impl drasi_core::interface::SourceMiddleware for MarkInput {
    async fn process(
        &self,
        mut change: SourceChange,
        _: &dyn drasi_core::interface::ElementIndex,
    ) -> std::result::Result<Vec<SourceChange>, drasi_core::interface::MiddlewareError> {
        let element = match &mut change {
            SourceChange::Insert { element } | SourceChange::Replace { element } => Some(element),
            SourceChange::Update {
                element,
                removed_properties,
            } => {
                removed_properties.remove("normalized");
                Some(element)
            }
            _ => None,
        };
        if let Some(Element::Node { properties, .. } | Element::Relation { properties, .. }) =
            element
        {
            properties.insert("normalized", drasi_core::models::ElementValue::Bool(true));
        }
        Ok(vec![change])
    }
}

#[derive(Default)]
struct Gate {
    blocked: AtomicBool,
    entered: Notify,
    release: Notify,
}

/// The fixture's own representation: counters and a bounded change log. It
/// stores no canonical records; providers derive graph records on demand.
struct Inventory {
    counts: Map<String, i64>,
    log: VecDeque<(u64, String, i64)>,
    sequence: u64,
    follow: OutputFollowHub,
}

impl Inventory {
    fn new(retention: usize) -> Self {
        Self {
            counts: Map::new(),
            log: VecDeque::new(),
            sequence: 0,
            follow: OutputFollowHub::new(NonZeroUsize::new(4).unwrap(), follow_limits(retention))
                .unwrap(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ReplayMode {
    Honest,
    /// Logical positions are retained but transport sequences change.
    Reemitted,
    /// Envelopes carry no producer progress marker.
    Unmarked,
    /// Envelopes carry another epoch's progress marker.
    ForeignEpoch,
}

struct Shared {
    replay_mode: StdMutex<ReplayMode>,
    advance_after_snapshot: AtomicBool,
    producer: GraphProducerIdentity,
    state: StdMutex<Inventory>,
    retention: usize,
    ready: AtomicBool,
    gate: Gate,
}

impl Shared {
    fn new(component: &str, epoch: u128, retention: usize) -> Arc<Self> {
        Self::reporting(component, component, epoch, retention)
    }

    fn persistent(component: &str, epoch: u128, retention: usize) -> Arc<Self> {
        let mut shared = Self::new(component, epoch, retention);
        Arc::get_mut(&mut shared).unwrap().producer = GraphProducerIdentity::for_epoch(
            "output-tests".into(),
            "output-tests".into(),
            ComponentId::try_new(component).unwrap(),
            StreamId::try_new(format!("{component}/out")).unwrap(),
            uuid::Uuid::from_u128(epoch),
            true,
        )
        .unwrap();
        shared
    }

    fn reporting(component: &str, reported: &str, epoch: u128, retention: usize) -> Arc<Self> {
        Arc::new(Self {
            replay_mode: StdMutex::new(ReplayMode::Honest),
            advance_after_snapshot: AtomicBool::new(false),
            producer: GraphProducerIdentity::for_epoch(
                "output-tests".into(),
                "output-tests".into(),
                ComponentId::try_new(reported).unwrap(),
                StreamId::try_new(format!("{component}/out")).unwrap(),
                uuid::Uuid::from_u128(epoch),
                false,
            )
            .unwrap(),
            state: StdMutex::new(Inventory::new(retention)),
            retention,
            ready: AtomicBool::new(true),
            gate: Gate::default(),
        })
    }

    fn apply(&self, key: &str, value: i64) {
        let mut state = self.state.lock().unwrap();
        state.sequence += 1;
        let sequence = state.sequence;
        state.counts.insert(key.into(), value);
        state.log.push_back((sequence, key.into(), value));
        state
            .follow
            .publish(&self.event(sequence, key, value).unwrap())
            .unwrap();
        while state.log.len() > self.retention {
            state.log.pop_front();
        }
    }

    fn metadata(key: &str) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new("inventory", key),
            labels: Arc::from(vec![Arc::<str>::from("Item")]),
            effective_from: 1,
        }
    }

    fn event(&self, sequence: u64, key: &str, value: i64) -> ReadResult<ChangeEnvelope> {
        let mut envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: Shared::metadata(key),
                    properties: ElementPropertyMap::from(serde_json::json!({ "count": value })),
                },
            },
            self.producer.stream().clone(),
            sequence,
            None,
        )
        .map_err(|error| OutputReadError::Failed(error.into()))?;
        GraphProducerProgress::annotate(&mut envelope, &self.producer, sequence)
            .map_err(OutputReadError::Failed)?;
        Ok(envelope)
    }

    async fn wait_gate(&self) {
        if self.gate.blocked.load(Ordering::Acquire) {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
    }

    fn head(&self, state: &Inventory) -> ReadResult<OutputCursor> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(OutputReadError::NotReady("fixture is not ready".into()));
        }
        OutputCursor::try_new(self.producer.clone(), state.sequence)
    }
}

struct InventorySnapshot(Arc<Shared>);
struct InventoryReplay(Arc<Shared>);

fn inventory_snapshot(
    cursor: OutputCursor,
    counts: &Map<String, i64>,
) -> ReadResult<OutputSnapshot> {
    let mut records = im::HashMap::new();
    for (key, value) in counts {
        let record = GraphChangeCodec::element_record(&ElementImage::Node {
            metadata: Shared::metadata(key),
            properties: Map::from([("count".to_string(), DataValue::Signed(*value))]),
        })
        .map_err(|error| OutputReadError::Failed(error.into()))?;
        records.insert(record.identity().clone(), record);
    }
    OutputSnapshot::try_new(
        cursor,
        "inventory-dataset",
        GraphChangeCodec::schema().descriptor().clone(),
        records,
    )
}

#[async_trait]
impl SnapshotProvider for InventorySnapshot {
    async fn snapshot(&self) -> ReadResult<OutputSnapshot> {
        self.0.wait_gate().await;
        let (cursor, counts) = {
            let state = self.0.state.lock().unwrap();
            (self.0.head(&state)?, state.counts.clone())
        };
        if self.0.advance_after_snapshot.swap(false, Ordering::AcqRel) {
            self.0.apply("changed-during-snapshot", 1);
        }
        inventory_snapshot(cursor, &counts)
    }
}

#[async_trait]
impl SnapshotFollowProvider for InventorySnapshot {
    async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> ReadResult<OutputSnapshotFollow> {
        self.0.wait_gate().await;
        let mut state = self.0.state.lock().unwrap();
        let snapshot = inventory_snapshot(self.0.head(&state)?, &state.counts)?;
        state.follow.open(snapshot, limits)
    }
}

#[async_trait]
impl ReplayProvider for InventoryReplay {
    async fn committed_head(&self) -> ReadResult<OutputCursor> {
        let state = self.0.state.lock().unwrap();
        self.0.head(&state)
    }

    async fn replay_after(&self, after: &OutputCursor) -> ReadResult<OutputReplay> {
        let (head, retained) = {
            let state = self.0.state.lock().unwrap();
            let head = self.0.head(&state)?;
            after.ensure_same_epoch(&head)?;
            let oldest = state.log.front().map(|entry| entry.0);
            if after.sequence() > head.sequence()
                || (after.sequence() < head.sequence()
                    && oldest.map_or(true, |oldest| after.sequence() + 1 < oldest))
            {
                return Err(OutputReadError::Gap {
                    requested: after.sequence(),
                    oldest,
                    latest: head.sequence(),
                });
            }
            let retained: Vec<_> = state
                .log
                .iter()
                .filter(|entry| entry.0 > after.sequence())
                .cloned()
                .collect();
            (head, retained)
        };
        let mode = *self.0.replay_mode.lock().unwrap();
        let foreign = GraphProducerIdentity::for_epoch(
            self.0.producer.construction_scope().into(),
            self.0.producer.graph_id().into(),
            self.0.producer.component_id().clone(),
            self.0.producer.stream().clone(),
            uuid::Uuid::from_u128(777),
            false,
        )
        .unwrap();
        let mut envelopes = Vec::new();
        for (sequence, key, value) in retained {
            let mut envelope = GraphChangeCodec::encode_change(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: Shared::metadata(&key),
                        properties: ElementPropertyMap::from(serde_json::json!({ "count": value })),
                    },
                },
                self.0.producer.stream().clone(),
                sequence,
                None,
            )
            .map_err(|error| OutputReadError::Failed(error.into()))?;
            let stamp = |envelope: &mut ChangeEnvelope, identity: &GraphProducerIdentity| {
                GraphProducerProgress::annotate(envelope, identity, sequence)
                    .map_err(OutputReadError::Failed)
            };
            match mode {
                ReplayMode::Honest => stamp(&mut envelope, &self.0.producer)?,
                ReplayMode::Reemitted => {
                    stamp(&mut envelope, &self.0.producer)?;
                    envelope = envelope
                        .reemit(sequence + 1_000)
                        .map_err(|error| OutputReadError::Failed(error.into()))?;
                }
                ReplayMode::Unmarked => {}
                ReplayMode::ForeignEpoch => stamp(&mut envelope, &foreign)?,
            }
            envelopes.push(envelope);
        }
        OutputReplay::try_new(after.clone(), head, envelopes)
    }
}

#[derive(Clone, Copy)]
struct Registration {
    snapshot: bool,
    replay: bool,
}

const BOTH: Registration = Registration {
    snapshot: true,
    replay: true,
};

fn readers(shared: &Arc<Shared>, port: &str, registration: Registration) -> OutputReaders {
    let port = PortId::try_new(port).unwrap();
    let mut readers = OutputReaders::new();
    if registration.snapshot {
        readers = readers.with_snapshot(port.clone(), Arc::new(InventorySnapshot(shared.clone())));
    }
    if registration.replay {
        readers = readers.with_replay(port.clone(), Arc::new(InventoryReplay(shared.clone())));
    }
    if registration.snapshot && registration.replay {
        readers = readers.with_snapshot_follow(port, Arc::new(InventorySnapshot(shared.clone())));
    }
    readers
}

fn port(id: &str, direction: PortDirection) -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new(id).unwrap(),
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}

struct FixtureSource {
    descriptor: ComponentDescriptor,
    readers: OutputReaders,
}

fn fixture_source(
    id: &str,
    shared: &Arc<Shared>,
    registration: Registration,
    registered_port: &str,
) -> FixtureSource {
    fixture_source_with_schema(
        id,
        shared,
        registration,
        registered_port,
        GraphChangeCodec::schema().descriptor().clone(),
    )
}

fn fixture_source_with_schema(
    id: &str,
    shared: &Arc<Shared>,
    registration: Registration,
    registered_port: &str,
    schema: SchemaDescriptor,
) -> FixtureSource {
    FixtureSource {
        descriptor: ComponentDescriptor::try_new(
            ComponentId::try_new(id).unwrap(),
            vec![PortDescriptor::new(
                PortId::try_new("out").unwrap(),
                PortDirection::Output,
                schema,
                PipeRequirements::default(),
            )],
        )
        .unwrap(),
        readers: readers(shared, registered_port, registration),
    }
}

#[async_trait]
impl ComputationComponent for FixtureSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn output_readers(&self) -> OutputReaders {
        self.readers.clone()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for FixtureSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

struct FixtureTransformer {
    descriptor: ComponentDescriptor,
    readers: OutputReaders,
}

#[async_trait]
impl ComputationComponent for FixtureTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn output_readers(&self) -> OutputReaders {
        self.readers.clone()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for FixtureTransformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}

struct FixtureSink(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for FixtureSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for FixtureSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

fn endpoint(component: &str, port: &str) -> OutputEndpoint {
    OutputEndpoint::new(
        ComponentId::try_new(component).unwrap(),
        PortId::try_new(port).unwrap(),
    )
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("output reads must remain responsive")
}

async fn sink(core: &DrasiLib) -> ComponentHandle {
    if let Ok(existing) = core.computation_component("sink") {
        return existing;
    }
    core.add_computation_component(
        ComponentAddition::new(ConstructedComponent::sink(Box::new(FixtureSink(
            ComponentDescriptor::try_new(
                ComponentId::try_new("sink").unwrap(),
                vec![port("in", PortDirection::Input)],
            )
            .unwrap(),
        ))))
        .auto_start(false),
    )
    .await
    .unwrap()
}

async fn add_source(core: &DrasiLib, source: FixtureSource) -> ComponentHandle {
    let id = source.descriptor.id().to_string();
    let handle = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::source(Box::new(source)))
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out").unwrap(),
                    StreamId::try_new(format!("{id}/out")).unwrap(),
                ),
        )
        .await
        .unwrap();
    let sink = sink(core).await;
    connect(core, (&handle, "out"), (&sink, "in")).await;
    bounded(handle.wait_created()).await.unwrap();
    handle
}

async fn add_transformer(
    core: &DrasiLib,
    id: &str,
    shared: &Arc<Shared>,
    registration: Registration,
) -> ComponentHandle {
    let handle = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::transformer(Box::new(
                FixtureTransformer {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new(id).unwrap(),
                        vec![port("in", PortDirection::Input), port("out", PortDirection::Output)],
                    )
                    .unwrap(),
                    readers: readers(shared, "out", registration),
                },
            )))
            .auto_start(false)
            .bind_stream(
                PortId::try_new("out").unwrap(),
                StreamId::try_new(format!("{id}/out")).unwrap(),
            ),
        )
        .await
        .unwrap();
    let feed = add_source(
        core,
        fixture_source(
            &format!("feed-{id}"),
            &Shared::new(&format!("feed-{id}"), 99, 1),
            Registration {
                snapshot: false,
                replay: false,
            },
            "out",
        ),
    )
    .await;
    connect(core, (&feed, "out"), (&handle, "in")).await;
    let sink = sink(core).await;
    connect(core, (&handle, "out"), (&sink, "in")).await;
    bounded(handle.wait_created()).await.unwrap();
    handle
}

/// The one consumer used for every kind of output. It knows nothing about
/// queries, sources or transformers: only the common read interfaces.
async fn consume(handle: &OutputHandle) -> ReadResult<(usize, u64, usize)> {
    let caught_up = handle.catch_up().await?;
    assert_eq!(caught_up.replay.after(), caught_up.snapshot.cursor());
    assert!(caught_up.replay.envelopes().is_empty());
    let head = handle.committed_head().await?;
    let history = handle
        .replay_after(&caught_up.snapshot.cursor().at_sequence(0))
        .await?;
    assert_eq!(history.head(), &head);
    Ok((
        caught_up.snapshot.records().len(),
        head.sequence(),
        history.envelopes().len(),
    ))
}

#[tokio::test]
async fn source_and_transformer_outputs_use_the_same_catalogue_and_consumer_path() {
    let core = builder().build().await.unwrap();
    let source_state = Shared::new("inventory-source", 1, 16);
    let transformer_state = Shared::new("inventory-transformer", 2, 16);
    add_source(
        &core,
        fixture_source("inventory-source", &source_state, BOTH, "out"),
    )
    .await;
    add_transformer(&core, "inventory-transformer", &transformer_state, BOTH).await;
    source_state.apply("a", 1);
    source_state.apply("b", 2);
    source_state.apply("a", 3);
    transformer_state.apply("x", 10);
    transformer_state.apply("y", 20);

    let catalogue = core.output_catalogue().unwrap();
    for (component, expected) in
        [("inventory-source", (2, 3, 3)), ("inventory-transformer", (2, 2, 2))]
    {
        let handle = catalogue.resolve(&endpoint(component, "out")).unwrap();
        assert!(handle.provides_snapshot() && handle.provides_replay());
        assert_eq!(bounded(consume(&handle)).await.unwrap(), expected);
    }
    // The transformer's input port is not an output.
    assert!(matches!(
        catalogue.resolve(&endpoint("inventory-transformer", "in")),
        Err(OutputReadError::UnknownOutput(_))
    ));
    assert!(matches!(
        catalogue.describe(&endpoint("missing", "out")),
        Err(OutputReadError::UnknownOutput(_))
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn source_and_transformer_snapshots_use_the_same_bounded_live_handover() {
    let core = builder().build().await.unwrap();
    let source = Shared::new("source", 1, 16);
    let transformed = Shared::new("transformer", 2, 16);
    add_source(&core, fixture_source("source", &source, BOTH, "out")).await;
    add_transformer(&core, "transformer", &transformed, BOTH).await;
    let catalogue = core.output_catalogue().unwrap();
    let mut tails = Vec::new();
    for (id, state) in [("source", &source), ("transformer", &transformed)] {
        state.apply("a", 1);
        let handle = catalogue.resolve(&endpoint(id, "out")).unwrap();
        let (snapshot, follow) = handle
            .snapshot_and_follow(follow_limits(8))
            .await
            .unwrap()
            .into_parts();
        assert_eq!(snapshot.records().len(), 1);
        assert_eq!(snapshot.cursor().sequence(), 1);
        state.apply("b", 2);
        state.apply("a", 3);
        assert_eq!(snapshot.records().len(), 1);
        tails.push((id, follow));
    }
    for (id, follow) in &mut tails {
        for sequence in [2, 3] {
            let envelope = bounded(follow.next()).await.unwrap();
            let progress = GraphProducerProgress::from_envelope(&envelope)
                .unwrap()
                .unwrap();
            assert_eq!(progress.identity().component_id().as_str(), *id);
            assert_eq!(progress.sequence(), sequence);
        }
    }
    core.shutdown().await.unwrap();
    for (_, mut follow) in tails {
        assert!(matches!(
            bounded(follow.next()).await,
            Err(OutputReadError::Retired)
        ));
    }
}

#[tokio::test]
async fn snapshot_and_replay_are_independent_and_absence_is_explicit() {
    let core = builder().build().await.unwrap();
    let state = Shared::new("snapshot-only", 1, 16);
    state.apply("a", 1);
    add_source(
        &core,
        fixture_source(
            "snapshot-only",
            &state,
            Registration {
                snapshot: true,
                replay: false,
            },
            "out",
        ),
    )
    .await;
    add_source(
        &core,
        fixture_source(
            "replay-only",
            &Shared::new("replay-only", 2, 16),
            Registration {
                snapshot: false,
                replay: true,
            },
            "out",
        ),
    )
    .await;
    add_source(
        &core,
        fixture_source(
            "plain",
            &Shared::new("plain", 3, 16),
            Registration {
                snapshot: false,
                replay: false,
            },
            "out",
        ),
    )
    .await;
    let catalogue = core.output_catalogue().unwrap();
    let provisions = |id: &str| {
        let description = catalogue.describe(&endpoint(id, "out")).unwrap();
        (description.snapshot, description.replay)
    };
    assert_eq!(
        provisions("snapshot-only"),
        (Provision::Provided, Provision::Absent)
    );
    assert_eq!(
        provisions("replay-only"),
        (Provision::Absent, Provision::Provided)
    );
    assert_eq!(provisions("plain"), (Provision::Absent, Provision::Absent));
    assert_eq!(
        catalogue
            .list()
            .unwrap()
            .iter()
            .filter(|description| description.snapshot == Provision::Provided)
            .count(),
        1
    );

    let snapshot_only = catalogue
        .resolve(&endpoint("snapshot-only", "out"))
        .unwrap();
    assert_eq!(snapshot_only.snapshot().await.unwrap().records().len(), 1);
    assert!(matches!(
        snapshot_only.committed_head().await,
        Err(OutputReadError::Absent {
            capability: OutputCapability::Replay,
            ..
        })
    ));
    let replay_only = catalogue.resolve(&endpoint("replay-only", "out")).unwrap();
    assert!(matches!(
        replay_only.snapshot().await,
        Err(OutputReadError::Absent {
            capability: OutputCapability::Snapshot,
            ..
        })
    ));
    assert_eq!(replay_only.committed_head().await.unwrap().sequence(), 0);
    assert!(matches!(
        replay_only.catch_up().await,
        Err(OutputReadError::Absent { .. })
    ));
    let plain = catalogue.resolve(&endpoint("plain", "out")).unwrap();
    assert!(!plain.provides_snapshot() && !plain.provides_replay());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn providers_report_typed_not_ready_gap_epoch_and_contract_errors() {
    let core = builder().build().await.unwrap();
    let state = Shared::new("source", 1, 2);
    add_source(&core, fixture_source("source", &state, BOTH, "out")).await;
    let liar = Shared::reporting("liar", "another-component", 9, 2);
    add_source(&core, fixture_source("liar", &liar, BOTH, "out")).await;
    let catalogue = core.output_catalogue().unwrap();
    let handle = catalogue.resolve(&endpoint("source", "out")).unwrap();

    state.ready.store(false, Ordering::Release);
    assert!(matches!(
        handle.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    assert!(matches!(
        handle.committed_head().await,
        Err(OutputReadError::NotReady(_))
    ));
    state.ready.store(true, Ordering::Release);

    for value in 1..=4 {
        state.apply("a", value);
    }
    let head = handle.committed_head().await.unwrap();
    assert_eq!(head.sequence(), 4);
    assert!(matches!(
        handle.replay_after(&head.at_sequence(0)).await,
        Err(OutputReadError::Gap {
            requested: 0,
            oldest: Some(3),
            latest: 4
        })
    ));
    assert!(matches!(
        handle.replay_after(&head.at_sequence(5)).await,
        Err(OutputReadError::Gap { .. })
    ));
    let retained = handle.replay_after(&head.at_sequence(2)).await.unwrap();
    assert_eq!(
        retained
            .envelopes()
            .iter()
            .map(|envelope| envelope.system().sequence())
            .collect::<Vec<_>>(),
        vec![3, 4]
    );

    let other_epoch = OutputCursor::try_new(
        GraphProducerIdentity::for_epoch(
            "output-tests".into(),
            "output-tests".into(),
            ComponentId::try_new("source").unwrap(),
            StreamId::try_new("source/out").unwrap(),
            uuid::Uuid::from_u128(2),
            false,
        )
        .unwrap(),
        4,
    )
    .unwrap();
    assert!(matches!(
        handle.replay_after(&other_epoch).await,
        Err(OutputReadError::EpochChanged { .. })
    ));

    liar.apply("a", 1);
    let liar = catalogue.resolve(&endpoint("liar", "out")).unwrap();
    assert!(matches!(
        liar.snapshot().await,
        Err(OutputReadError::Invalid(_))
    ));
    core.shutdown().await.unwrap();
}

const UNDECLARED: &str = "undeclared output port";

fn bad_source(state: &Arc<Shared>) -> FixtureSource {
    fixture_source("bad", state, BOTH, "missing")
}

#[tokio::test]
async fn invalid_registrations_are_rejected_at_addition_and_build() {
    let core = builder().build().await.unwrap();
    let state = Shared::new("bad", 1, 2);
    let rejected = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::source(Box::new(bad_source(&state))))
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out").unwrap(),
                    StreamId::try_new("bad/out").unwrap(),
                ),
        )
        .await;
    assert!(chain(&rejected.expect_err("addition must be rejected")).contains(UNDECLARED));
    assert!(core.computation_component("bad").is_err());
    assert!(matches!(
        core.output_catalogue()
            .unwrap()
            .describe(&endpoint("bad", "out")),
        Err(OutputReadError::UnknownOutput(_))
    ));
    core.shutdown().await.unwrap();

    let built = ComputationGraph::builder("invalid-readers")
        .source(Box::new(bad_source(&state)))
        .build();
    assert!(chain(&built.err().expect("build must be rejected")).contains(UNDECLARED));
}

#[tokio::test]
async fn an_invalid_replacement_is_rejected_before_retiring_the_current_output() {
    let fixture = started_fixture(1, 1).await;
    let control = fixture.core.computation_control().unwrap();
    let catalogue = fixture.core.output_catalogue().unwrap();
    let current = catalogue.resolve(&endpoint("native", "out")).unwrap();

    let desired = control.desired_snapshot();
    let replacement = desired
        .select(GraphSelection::Exact(vec![fixture.handle.id().clone()]))
        .unwrap()
        .components
        .remove(0);
    let ComponentConstruction::External { binding } = &replacement.construction else {
        panic!("native external source")
    };
    let mut bindings = TopologyBindings::default();
    bindings.components.insert(
        binding.clone(),
        ConstructedComponent::source(Box::new(fixture_source(
            "native",
            &Shared::new("native", 2, 8),
            BOTH,
            "missing",
        ))),
    );
    let plan = control
        .preview(
            desired.revision,
            vec![DesiredMutation::ReplaceComponent(replacement)],
        )
        .await
        .unwrap();
    let error = bounded(control.reconcile(plan, bindings)).await;
    assert!(chain(&error.expect_err("replacement must fail")).contains(UNDECLARED));
    assert_eq!(current.snapshot().await.unwrap().records().len(), 1);
    fixture.core.shutdown().await.unwrap();
}

/// Valid when first inspected, invalid when its slot later captures it.
struct ChangingRegistration {
    inner: FixtureSource,
    calls: AtomicUsize,
    changed: FixtureSource,
}

#[async_trait]
impl ComputationComponent for ChangingRegistration {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.inner.descriptor
    }
    fn output_readers(&self) -> OutputReaders {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            self.inner.readers.clone()
        } else {
            self.changed.readers.clone()
        }
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for ChangingRegistration {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_registration_that_changes_after_validation_fails_creation_explicitly() {
    let core = builder().build().await.unwrap();
    let state = Shared::new("flaky", 1, 2);
    let handle = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::source(Box::new(
                ChangingRegistration {
                    inner: fixture_source("flaky", &state, BOTH, "out"),
                    calls: AtomicUsize::new(0),
                    changed: fixture_source("flaky", &state, BOTH, "missing"),
                },
            )))
            .auto_start(false)
            .bind_stream(
                PortId::try_new("out").unwrap(),
                StreamId::try_new("flaky/out").unwrap(),
            ),
        )
        .await
        .unwrap();
    let sink = sink(&core).await;
    // The damaged generation reports its cause from whichever operation first
    // touches it: connecting its pipe or creating it.
    let message = match try_connect(&core, (&handle, "out"), (&sink, "in")).await {
        Err(error) => chain(&error),
        Ok(report) => match bounded(handle.wait_created()).await {
            Err(error) => format!("{report:?} {}", chain(&error)),
            Ok(()) => panic!("a damaged generation must not be created"),
        },
    };
    assert!(message.contains(UNDECLARED), "{message}");
    let description = core
        .output_catalogue()
        .unwrap()
        .describe(&endpoint("flaky", "out"))
        .unwrap();
    assert_ne!(description.snapshot, Provision::Provided);
    core.shutdown().await.ok();
}

#[tokio::test]
async fn replay_validates_logical_progress_epoch_and_reemission() {
    let core = builder().build().await.unwrap();
    let state = Shared::new("source", 1, 8);
    for value in 1..=3 {
        state.apply("a", value);
    }
    add_source(&core, fixture_source("source", &state, BOTH, "out")).await;
    let handle = core
        .output_catalogue()
        .unwrap()
        .resolve(&endpoint("source", "out"))
        .unwrap();
    let from_start = handle.committed_head().await.unwrap().at_sequence(0);

    let logical = |replay: &OutputReplay| {
        replay
            .envelopes()
            .iter()
            .map(|envelope| {
                GraphProducerProgress::from_envelope(envelope)
                    .unwrap()
                    .unwrap()
                    .sequence()
            })
            .collect::<Vec<_>>()
    };
    let honest = handle.replay_after(&from_start).await.unwrap();
    assert_eq!(logical(&honest), vec![1, 2, 3]);

    // Re-emission changes transport positions, never logical ones.
    *state.replay_mode.lock().unwrap() = ReplayMode::Reemitted;
    let reemitted = handle.replay_after(&from_start).await.unwrap();
    assert_eq!(logical(&reemitted), vec![1, 2, 3]);
    assert_eq!(
        reemitted
            .envelopes()
            .iter()
            .map(|envelope| envelope.system().sequence())
            .collect::<Vec<_>>(),
        vec![1_001, 1_002, 1_003]
    );

    for forged in [ReplayMode::Unmarked, ReplayMode::ForeignEpoch] {
        *state.replay_mode.lock().unwrap() = forged;
        assert!(matches!(
            handle.replay_after(&from_start).await,
            Err(OutputReadError::Invalid(_))
        ));
        state.advance_after_snapshot.store(true, Ordering::Release);
        assert!(matches!(
            handle.catch_up().await,
            Err(OutputReadError::Invalid(_))
        ));
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshot_and_replay_must_use_the_ports_declared_schema() {
    let core = builder().build().await.unwrap();
    let state = Shared::new("mismatch", 1, 8);
    state.apply("a", 1);
    let other_schema = SchemaDescriptor::try_new(
        SchemaId::try_new("test.other-format").unwrap(),
        SchemaVersion::try_new(1).unwrap(),
        "test-only",
        bytes::Bytes::from_static(b"not-the-graph-contract"),
    )
    .unwrap();
    assert_ne!(&other_schema, GraphChangeCodec::schema().descriptor());
    // A genuinely different format, not the query helper's shared graph schema.
    core.add_computation_component(
        ComponentAddition::new(ConstructedComponent::source(Box::new(
            fixture_source_with_schema("mismatch", &state, BOTH, "out", other_schema),
        )))
        .auto_start(false)
        .bind_stream(
            PortId::try_new("out").unwrap(),
            StreamId::try_new("mismatch/out").unwrap(),
        ),
    )
    .await
    .unwrap();
    let handle = core
        .output_catalogue()
        .unwrap()
        .resolve(&endpoint("mismatch", "out"))
        .unwrap();
    assert!(matches!(
        handle.snapshot().await,
        Err(OutputReadError::Invalid(_))
    ));
    let from_start = OutputCursor::try_new(state.producer.clone(), 0).unwrap();
    assert!(matches!(
        handle.replay_after(&from_start).await,
        Err(OutputReadError::Invalid(_))
    ));
    core.shutdown().await.unwrap();
}

struct Fixture {
    core: Arc<DrasiLib>,
    state: Arc<Shared>,
    snapshot_provider: std::sync::Weak<dyn SnapshotProvider>,
    handle: ComponentHandle,
}

async fn started_fixture(epoch: u128, value: i64) -> Fixture {
    started_fixture_with(epoch, value, BOTH).await
}

async fn started_fixture_with(epoch: u128, value: i64, registration: Registration) -> Fixture {
    let core = Arc::new(builder().build().await.unwrap());
    let state = Shared::new("native", epoch, 8);
    state.apply("a", value);
    let source = fixture_source("native", &state, registration, "out");
    let snapshot_provider = source
        .readers
        .ports()
        .values()
        .next()
        .and_then(|readers| readers.snapshot())
        .map(Arc::downgrade)
        .unwrap_or_else(|| std::sync::Weak::<InventorySnapshot>::new());
    let handle = add_source(&core, source).await;
    bounded(sink(&core).await.start()).await.unwrap();
    bounded(handle.start()).await.unwrap();
    Fixture {
        core,
        state,
        snapshot_provider,
        handle,
    }
}

async fn try_connect(
    core: &DrasiLib,
    from: (&ComponentHandle, &str),
    to: (&ComponentHandle, &str),
) -> GraphResult<ReconciliationReport> {
    let control = core.computation_control().unwrap();
    let plan = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::Bind(DesiredRelationship {
                definition: EdgeDefinition {
                    from: Endpoint::new(from.0.id().clone(), PortId::try_new(from.1).unwrap()),
                    to: Endpoint::new(to.0.id().clone(), PortId::try_new(to.1).unwrap()),
                },
                policy: Default::default(),
                pipe: DesiredPipe::Bounded { capacity: 2 },
            })],
        )
        .await?;
    control.reconcile(plan, TopologyBindings::default()).await
}

async fn connect(core: &DrasiLib, from: (&ComponentHandle, &str), to: (&ComponentHandle, &str)) {
    let report = try_connect(core, from, to).await.unwrap();
    // A component with several ports is created only once all are connected.
    assert!(report.failures.is_empty() && report.committed, "{report:?}");
}

/// A component's complete error message including every nested cause.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

async fn pending_snapshot(
    fixture: &Fixture,
    handle: &OutputHandle,
) -> tokio::task::JoinHandle<ReadResult<OutputSnapshot>> {
    fixture.state.gate.blocked.store(true, Ordering::Release);
    let handle = handle.clone();
    let read = tokio::spawn(async move { handle.snapshot().await });
    bounded(fixture.state.gate.entered.notified()).await;
    read
}

fn assert_retired<T: std::fmt::Debug>(result: ReadResult<T>) {
    assert!(
        matches!(result, Err(OutputReadError::Retired)),
        "{result:?}"
    );
}

#[tokio::test]
async fn graph_state_changes_retire_pending_reads_without_a_registry_update() {
    let fixture = started_fixture(1, 1).await;
    let control = fixture.core.computation_control().unwrap();
    let (state, changes) = tokio::sync::watch::channel(GraphState::Running);
    let catalogue = OutputCatalogue::from_publications(control.subscribe_registry(), changes);
    let output = catalogue.resolve(&endpoint("native", "out")).unwrap();
    let pending = pending_snapshot(&fixture, &output).await;
    state.send_replace(GraphState::CleanupRequired);
    assert_retired(bounded(pending).await.unwrap());
    fixture.state.gate.blocked.store(false, Ordering::Release);
    assert_eq!(
        fixture
            .core
            .output_catalogue()
            .unwrap()
            .resolve(&endpoint("native", "out"))
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .records()
            .len(),
        1,
    );
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_and_restart_retire_existing_handles_and_pending_reads() {
    let fixture = started_fixture(1, 1).await;
    let catalogue = fixture.core.output_catalogue().unwrap();
    let old = catalogue.resolve(&endpoint("native", "out")).unwrap();
    assert_eq!(old.snapshot().await.unwrap().records().len(), 1);

    let pending = pending_snapshot(&fixture, &old).await;
    bounded(fixture.core.stop_source("native")).await.unwrap();
    assert_retired(bounded(pending).await.unwrap());
    fixture.state.gate.blocked.store(false, Ordering::Release);
    assert_retired(old.snapshot().await);
    assert_retired(old.committed_head().await);

    bounded(fixture.core.start_source("native")).await.unwrap();
    assert_retired(old.snapshot().await);
    let current = catalogue.resolve(&endpoint("native", "out")).unwrap();
    assert_eq!(current.snapshot().await.unwrap().records().len(), 1);
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn absent_capabilities_are_retired_with_their_generation_not_reported_absent() {
    let fixture = started_fixture_with(
        1,
        1,
        Registration {
            snapshot: false,
            replay: false,
        },
    )
    .await;
    let catalogue = fixture.core.output_catalogue().unwrap();
    let handle = catalogue.resolve(&endpoint("native", "out")).unwrap();
    assert!(matches!(
        handle.snapshot().await,
        Err(OutputReadError::Absent { .. })
    ));
    assert!(matches!(
        handle.committed_head().await,
        Err(OutputReadError::Absent { .. })
    ));

    bounded(fixture.core.stop_source("native")).await.unwrap();
    assert_retired(handle.snapshot().await);
    assert_retired(handle.committed_head().await);
    assert_retired(
        handle
            .replay_after(&old_cursor(&uuid::Uuid::from_u128(1)))
            .await,
    );
    assert_retired(handle.catch_up().await);
    bounded(fixture.core.start_source("native")).await.unwrap();
    assert_retired(handle.snapshot().await);
    let current = catalogue.resolve(&endpoint("native", "out")).unwrap();
    assert!(matches!(
        current.snapshot().await,
        Err(OutputReadError::Absent { .. })
    ));
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn replacement_never_serves_the_replacements_data_through_an_old_handle() {
    let fixture = started_fixture(1, 1).await;
    let control = fixture.core.computation_control().unwrap();
    let catalogue = fixture.core.output_catalogue().unwrap();
    let old = catalogue.resolve(&endpoint("native", "out")).unwrap();
    let old_epoch = old.snapshot().await.unwrap().cursor().epoch();

    let desired = control.desired_snapshot();
    let replacement = desired
        .select(GraphSelection::Exact(vec![fixture.handle.id().clone()]))
        .unwrap()
        .components
        .remove(0);
    let ComponentConstruction::External { binding } = &replacement.construction else {
        panic!("native external source")
    };
    let successor = Shared::new("native", 2, 8);
    successor.apply("a", 1);
    successor.apply("b", 2);
    let mut bindings = TopologyBindings::default();
    bindings.components.insert(
        binding.clone(),
        ConstructedComponent::source(Box::new(fixture_source("native", &successor, BOTH, "out"))),
    );
    let plan = control
        .preview(
            desired.revision,
            vec![DesiredMutation::ReplaceComponent(replacement)],
        )
        .await
        .unwrap();
    assert_eq!(
        bounded(control.reconcile(plan, bindings))
            .await
            .unwrap()
            .summary,
        OperationSummary::Completed
    );

    assert_retired(old.snapshot().await);
    assert_retired(old.replay_after(&old_cursor(&old_epoch)).await);
    assert!(fixture.snapshot_provider.upgrade().is_none());
    let current = catalogue.resolve(&endpoint("native", "out")).unwrap();
    let snapshot = current.snapshot().await.unwrap();
    assert_eq!(snapshot.records().len(), 2);
    assert_ne!(snapshot.cursor().epoch(), old_epoch);
    fixture.core.shutdown().await.unwrap();
}

fn old_cursor(epoch: &uuid::Uuid) -> OutputCursor {
    OutputCursor::try_new(
        GraphProducerIdentity::for_epoch(
            "output-tests".into(),
            "output-tests".into(),
            ComponentId::try_new("native").unwrap(),
            StreamId::try_new("native/out").unwrap(),
            *epoch,
            false,
        )
        .unwrap(),
        0,
    )
    .unwrap()
}

#[tokio::test]
async fn removal_and_shutdown_retire_handles_and_release_providers() {
    let fixture = started_fixture(1, 1).await;
    let catalogue = fixture.core.output_catalogue().unwrap();
    let old = catalogue.resolve(&endpoint("native", "out")).unwrap();
    let pending = pending_snapshot(&fixture, &old).await;

    let control = fixture.core.computation_control().unwrap();
    let plan = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(vec![fixture.handle.id().clone()]),
                policy: RemovalPolicy::Cascade,
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        bounded(control.reconcile(plan, TopologyBindings::default()))
            .await
            .unwrap()
            .summary,
        OperationSummary::Completed
    );
    assert_retired(bounded(pending).await.unwrap());
    assert_retired(old.snapshot().await);
    assert!(fixture.snapshot_provider.upgrade().is_none());
    assert!(matches!(
        catalogue.resolve(&endpoint("native", "out")),
        Err(OutputReadError::UnknownOutput(_))
    ));
    fixture.core.shutdown().await.unwrap();

    let fixture = started_fixture(3, 1).await;
    let handle = fixture
        .core
        .output_catalogue()
        .unwrap()
        .resolve(&endpoint("native", "out"))
        .unwrap();
    let pending = pending_snapshot(&fixture, &handle).await;
    bounded(fixture.core.shutdown()).await.unwrap();
    assert_retired(bounded(pending).await.unwrap());
}

async fn wait_for_records(handle: &OutputHandle, count: usize) -> OutputSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match handle.snapshot().await {
                Ok(snapshot) if snapshot.records().len() == count => return snapshot,
                Ok(_) | Err(OutputReadError::NotReady(_)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => panic!("unexpected output read error: {error:?}"),
            }
        }
    })
    .await
    .expect("query output must reach the expected rows")
}

fn query_config(capacity: usize) -> QueryConfig {
    crate::Query::cypher("query")
        .query("MATCH (n:Person) RETURN n.name AS name")
        .from_input("source")
        .auto_start(false)
        .enable_bootstrap(false)
        .with_outbox_capacity(capacity)
        .build()
}

async fn running_query(capacity: usize) -> Arc<DrasiLib> {
    let (source, _) = ControlledSource::new("source");
    let core = Arc::new(builder().build().await.unwrap());
    core.add_source(source).await.unwrap();
    core.add_query(query_config(capacity)).await.unwrap();
    core
}

#[tokio::test]
async fn query_outputs_use_the_common_providers_over_existing_query_state() {
    let core = running_query(8).await;
    let catalogue = core.output_catalogue().unwrap();
    let query = endpoint("query", "out");
    let description = catalogue.describe(&query).unwrap();
    assert!(matches!(
        description.snapshot,
        Provision::Provided | Provision::NotConstructed
    ));

    // Not started: typed not-ready, never an empty snapshot.
    if let Ok(handle) = catalogue.resolve(&query) {
        assert!(matches!(
            handle.snapshot().await,
            Err(OutputReadError::NotReady(_))
        ));
        assert!(matches!(
            handle.committed_head().await,
            Err(OutputReadError::NotReady(_))
        ));
    }

    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let handle = catalogue.resolve(&query).unwrap();
    assert!(handle.provides_snapshot() && handle.provides_replay());
    assert_eq!(wait_for_records(&handle, 0).await.records().len(), 0);

    insert_person(&core, "source", "one", "Alice").await;
    let first = wait_for_records(&handle, 1).await;
    assert_eq!(first.cursor().producer().component_id().as_str(), "query");
    assert!(first.cursor().sequence() > 0);
    let (_, head_sequence, history) = consume(&handle).await.unwrap();
    assert_eq!(head_sequence, first.cursor().sequence());
    assert_eq!(history as u64, head_sequence);

    insert_person(&core, "source", "two", "Bob").await;
    let second = wait_for_records(&handle, 2).await;
    let suffix = handle.replay_after(first.cursor()).await.unwrap();
    assert_eq!(
        suffix
            .envelopes()
            .iter()
            .map(|envelope| envelope.system().sequence())
            .collect::<Vec<_>>(),
        (first.cursor().sequence() + 1..=second.cursor().sequence()).collect::<Vec<_>>()
    );
    // The legacy facade reads the same query state.
    assert_eq!(core.get_query_results("query").await.unwrap().len(), 2);

    let other_epoch = OutputCursor::try_new(
        GraphProducerIdentity::for_epoch(
            "output-tests".into(),
            "output-tests".into(),
            ComponentId::try_new("query").unwrap(),
            first.cursor().producer().stream().clone(),
            uuid::Uuid::new_v4(),
            first.cursor().producer().persistent(),
        )
        .unwrap(),
        0,
    )
    .unwrap();
    assert!(matches!(
        handle.replay_after(&other_epoch).await,
        Err(OutputReadError::EpochChanged { .. })
    ));

    core.stop_query("query").await.unwrap();
    assert_retired(handle.snapshot().await);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_replay_reports_an_explicit_gap_when_retention_is_exceeded() {
    let core = running_query(1).await;
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let handle = core
        .output_catalogue()
        .unwrap()
        .resolve(&endpoint("query", "out"))
        .unwrap();
    for (id, name) in [("one", "Alice"), ("two", "Bob"), ("three", "Carol")] {
        insert_person(&core, "source", id, name).await;
    }
    let snapshot = wait_for_records(&handle, 3).await;
    assert!(matches!(
        handle.replay_after(&snapshot.cursor().at_sequence(0)).await,
        Err(OutputReadError::Gap { .. })
    ));
    assert!(handle
        .replay_after(&snapshot.cursor().clone())
        .await
        .unwrap()
        .envelopes()
        .is_empty());
    core.shutdown().await.unwrap();
}

fn follow_limits(capacity: usize) -> OutputFollowLimits {
    OutputFollowLimits {
        max_envelopes: NonZeroUsize::new(capacity).unwrap(),
        max_bytes: NonZeroUsize::new(1024 * 1024).unwrap(),
        lifetime: Duration::from_secs(30),
    }
}

#[tokio::test]
async fn query_input_middleware_uses_the_upstream_component_for_snapshot_and_live_data() {
    let mut middleware = drasi_core::middleware::MiddlewareTypeRegistry::new();
    middleware.register(Arc::new(MarkInputFactory));
    let config = crate::config::RuntimeConfig::new(
        crate::config::DrasiLibConfig::default(),
        Default::default(),
        None,
        None,
        None,
        None,
        None,
    );
    let mut core = DrasiLib::new_with_middleware(Arc::new(config), Arc::new(middleware));
    core.initialize().await.unwrap();
    let (source, _) = ControlledSource::new("source");
    core.add_source(source).await.unwrap();
    let mut upstream = query_config(8);
    upstream.query = "MATCH (p:Person) WHERE p.normalized = true RETURN p.name AS name".into();
    upstream.inputs[0].pipeline = vec!["mark".into()];
    upstream.middleware = vec![drasi_core::models::SourceMiddlewareConfig::new(
        "mark-input",
        "mark",
        Default::default(),
    )];
    core.add_query(upstream).await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let catalogue = core.output_catalogue().unwrap();
    let first = catalogue.resolve(&endpoint("query", "out")).unwrap();
    insert_person(&core, "source", "one", "Alice").await;
    let initial = wait_for_records(&first, 1).await;
    assert!(initial.dataset().starts_with("drasi.output/"));
    core.add_query(
        crate::Query::cypher("normalized")
            .query("MATCH (r:Result) RETURN r.name AS name, r.normalized AS normalized")
            .from_input_with_pipeline("query", vec!["mark".into()])
            .with_middleware(drasi_core::models::SourceMiddlewareConfig::new(
                "mark-input",
                "mark",
                Default::default(),
            ))
            .auto_start(false)
            .build(),
    )
    .await
    .unwrap();
    core.start_query("normalized").await.unwrap();
    core.computation_component("normalized")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let output = catalogue.resolve(&endpoint("normalized", "out")).unwrap();
    wait_for_records(&output, 1).await;
    assert_eq!(
        core.get_query_results("normalized").await.unwrap(),
        [serde_json::json!({"name":"Alice","normalized":true})]
    );
    insert_person(&core, "source", "two", "Bob").await;
    wait_for_records(&output, 2).await;
    let mut rows = core.get_query_results("normalized").await.unwrap();
    rows.sort_by_key(|row| row["name"].as_str().unwrap().to_owned());
    assert_eq!(
        rows,
        [
            serde_json::json!({"name":"Alice","normalized":true}),
            serde_json::json!({"name":"Bob","normalized":true}),
        ]
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_query_inputs_bootstrap_from_query_outputs_and_follow_live_changes() {
    let core = running_query(8).await;
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let catalogue = core.output_catalogue().unwrap();
    let first = catalogue.resolve(&endpoint("query", "out")).unwrap();
    insert_person(&core, "source", "one", "Alice").await;
    wait_for_records(&first, 1).await;

    core.add_query(
        crate::Query::cypher("derived")
            .query("MATCH (r:Result) RETURN r.name AS name")
            .from_input("query")
            .auto_start(false)
            .build(),
    )
    .await
    .unwrap();
    core.start_query("derived").await.unwrap();
    core.computation_component("derived")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let derived = catalogue.resolve(&endpoint("derived", "out")).unwrap();
    assert_eq!(wait_for_records(&derived, 1).await.records().len(), 1);
    assert_eq!(
        core.get_query_results("derived").await.unwrap(),
        [serde_json::json!({"name":"Alice"})]
    );

    insert_person(&core, "source", "two", "Bob").await;
    wait_for_records(&derived, 2).await;
    core.stop_query("derived").await.unwrap();
    insert_person(&core, "source", "three", "Carol").await;
    let first = catalogue.resolve(&endpoint("query", "out")).unwrap();
    wait_for_records(&first, 3).await;
    core.start_query("derived").await.unwrap();
    core.computation_component("derived")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let restarted = catalogue.resolve(&endpoint("derived", "out")).unwrap();
    wait_for_records(&restarted, 3).await;
    assert!(matches!(
        derived.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    core.update_query(
        "derived",
        crate::Query::cypher("derived")
            .query("MATCH (r:Result) WHERE r.name = 'Carol' RETURN r.name AS name")
            .from_input("query")
            .auto_start(false)
            .build(),
    )
    .await
    .unwrap();
    core.computation_component("derived")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let updated = catalogue.resolve(&endpoint("derived", "out")).unwrap();
    wait_for_records(&updated, 1).await;
    assert_eq!(
        core.get_query_results("derived").await.unwrap(),
        [serde_json::json!({"name":"Carol"})]
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_query_inputs_can_be_declared_before_the_upstream_query() {
    let core = builder().build().await.unwrap();
    let (source, _) = ControlledSource::new("source");
    core.add_source(source).await.unwrap();
    let waiting = core
        .add_query_with_handle(
            crate::Query::cypher("derived")
                .query("MATCH (r:Result) RETURN r.name AS name")
                .from_input("query")
                .auto_start(false)
                .build(),
        )
        .await
        .unwrap();
    assert_waiting_for(&waiting, "query");
    tokio::time::timeout(Duration::from_secs(5), core.add_query(query_config(8)))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "upstream addition stalled: {:?}",
                core.computation_control().unwrap().observed().components
            )
        })
        .unwrap();
    bounded(core.start_source("source")).await.unwrap();
    bounded(core.start_query("query")).await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let catalogue = core.output_catalogue().unwrap();
    let first = catalogue.resolve(&endpoint("query", "out")).unwrap();
    insert_person(&core, "source", "one", "Alice").await;
    wait_for_records(&first, 1).await;
    core.start_query("derived").await.unwrap();
    core.computation_component("derived")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let derived = catalogue.resolve(&endpoint("derived", "out")).unwrap();
    wait_for_records(&derived, 1).await;
    assert_eq!(
        core.get_query_results("derived").await.unwrap(),
        [serde_json::json!({"name":"Alice"})]
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn persistent_query_inputs_replay_missed_changes_and_report_retention_gaps() {
    let directory = tempfile::tempdir().unwrap();
    let mut middleware = drasi_core::middleware::MiddlewareTypeRegistry::new();
    middleware.register(Arc::new(MarkInputFactory));
    let provider: Arc<dyn drasi_core::interface::IndexBackendPlugin> = Arc::new(
        drasi_index_rocksdb::RocksDbIndexProvider::new(directory.path(), false, false),
    );
    let config = crate::config::RuntimeConfig::new(
        crate::config::DrasiLibConfig::default(),
        std::collections::HashMap::from([("persistent".into(), provider)]),
        None,
        None,
        None,
        None,
        Some(crate::indexes::StorageBackendRef::Named(
            "persistent".into(),
        )),
    );
    let mut core = DrasiLib::new_with_middleware(Arc::new(config), Arc::new(middleware));
    core.initialize().await.unwrap();
    let inventory = Shared::persistent("inventory", 1, 3);
    inventory.apply("a", 1);
    let source = add_source(&core, fixture_source("inventory", &inventory, BOTH, "out")).await;
    source.start().await.unwrap();
    let config = crate::Query::cypher("derived")
        .query("MATCH (n:Item) WHERE n.normalized = true RETURN n.count AS count")
        .from_input_with_pipeline("inventory", vec!["mark".into()])
        .with_middleware(drasi_core::models::SourceMiddlewareConfig::new(
            "mark-input",
            "mark",
            Default::default(),
        ))
        .auto_start(false)
        .build();
    core.add_query(config.clone()).await.unwrap();
    core.start_query("derived").await.unwrap();
    core.computation_component("derived")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let catalogue = core.output_catalogue().unwrap();
    let handle = catalogue.resolve(&endpoint("derived", "out")).unwrap();
    let original = wait_for_records(&handle, 1).await;

    core.stop_query("derived").await.unwrap();
    // The fixture exposes retained history but sends no live events while the
    // query is stopped, so these changes must come through ReplayProvider.
    inventory.apply("b", 2);
    inventory.apply("a", 3);
    core.start_query("derived").await.unwrap();
    core.computation_component("derived")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let handle = catalogue.resolve(&endpoint("derived", "out")).unwrap();
    let replayed = wait_for_records(&handle, 2).await;
    original
        .cursor()
        .ensure_same_epoch(replayed.cursor())
        .unwrap();
    let mut rows = core.get_query_results("derived").await.unwrap();
    rows.sort_by_key(|row| row["count"].as_i64());
    assert_eq!(
        rows,
        [serde_json::json!({"count":2}), serde_json::json!({"count":3})]
    );

    core.stop_query("derived").await.unwrap();
    for value in 4..=8 {
        inventory.apply("a", value);
    }
    let error = core
        .start_query("derived")
        .await
        .expect_err("retention gap must fail strict recovery");
    assert!(format!("{error:#}").contains("output history after 3 is unavailable"));
    core.stop_query("derived").await.unwrap();
    let mut reset = config;
    reset.recovery_policy = Some(crate::RecoveryPolicy::AutoReset);
    core.update_query("derived", reset).await.unwrap();
    core.start_query("derived").await.unwrap();
    core.computation_component("derived")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let handle = catalogue.resolve(&endpoint("derived", "out")).unwrap();
    let refreshed = wait_for_records(&handle, 2).await;
    assert!(matches!(
        replayed.cursor().ensure_same_epoch(refreshed.cursor()),
        Err(OutputReadError::EpochChanged { .. })
    ));
    let mut rows = core.get_query_results("derived").await.unwrap();
    rows.sort_by_key(|row| row["count"].as_i64());
    assert_eq!(
        rows,
        [serde_json::json!({"count":2}), serde_json::json!({"count":8})]
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_snapshot_follow_retains_changes_and_retires_with_the_generation() {
    let core = running_query(8).await;
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let catalogue = core.output_catalogue().unwrap();
    let handle = catalogue.resolve(&endpoint("query", "out")).unwrap();
    assert!(handle.provides_snapshot_follow());
    assert_eq!(
        catalogue
            .describe(handle.endpoint())
            .unwrap()
            .snapshot_follow,
        Provision::Provided
    );

    insert_person(&core, "source", "one", "Alice").await;
    wait_for_records(&handle, 1).await;
    let opened = handle.snapshot_and_follow(follow_limits(8)).await.unwrap();
    let position = opened.snapshot().cursor().clone();
    assert_eq!(opened.snapshot().records().len(), 1);
    insert_person(&core, "source", "two", "Bob").await;
    insert_person(&core, "source", "three", "Carol").await;
    let latest = wait_for_records(&handle, 3).await;
    let (snapshot, mut follow) = opened.into_parts();
    assert_eq!(snapshot.records().len(), 1);
    for sequence in position.sequence() + 1..=latest.cursor().sequence() {
        let envelope = bounded(follow.next()).await.unwrap();
        let progress = GraphProducerProgress::from_envelope(&envelope)
            .unwrap()
            .unwrap();
        assert_eq!(progress.identity(), position.producer());
        assert_eq!(progress.sequence(), sequence);
    }
    core.stop_query("query").await.unwrap();
    assert!(matches!(
        bounded(follow.next()).await,
        Err(OutputReadError::Retired)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_snapshot_follow_overflow_is_explicit_and_does_not_stall_queries() {
    let core = running_query(2).await;
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let handle = core
        .output_catalogue()
        .unwrap()
        .resolve(&endpoint("query", "out"))
        .unwrap();
    let (_, mut follow) = handle
        .snapshot_and_follow(follow_limits(1))
        .await
        .unwrap()
        .into_parts();
    insert_person(&core, "source", "one", "Alice").await;
    insert_person(&core, "source", "two", "Bob").await;
    wait_for_records(&handle, 2).await;
    assert!(matches!(
        bounded(follow.next()).await,
        Err(OutputReadError::Gap { .. })
    ));
    assert!(matches!(follow.next().await, Err(OutputReadError::Retired)));
    assert_eq!(core.get_query_results("query").await.unwrap().len(), 2);
    core.shutdown().await.unwrap();
}
