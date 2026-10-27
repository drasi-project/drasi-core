// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Non-query snapshot/replay providers served through the exported native
//! output-read tables: independent capabilities, typed errors, lifecycle fencing,
//! cancellation, retirement and negotiated bounds.

use super::*;
use async_trait::async_trait;
use drasi_computation_contracts::ElementImage;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk as sdk;
use drasi_core::models::{ElementMetadata, ElementReference};
use serde_json::json;
use std::result::Result;
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;

const DATASET: &str = "dataset";
const HUB_LEASES: usize = 4;

fn follow_limits(max_envelopes: usize, lifetime: Duration) -> OutputFollowLimits {
    OutputFollowLimits {
        max_envelopes: NonZeroUsize::new(max_envelopes).unwrap(),
        max_bytes: NonZeroUsize::new(1024 * 1024).unwrap(),
        lifetime,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Both,
    SnapshotOnly,
    ReplayOnly,
    Transformer,
    DeclaredNotRegistered,
    RegisteredNotDeclared,
    UnknownPort,
    DuplicatePort,
    InputPort,
    /// Snapshot, replay and follow providers on one output.
    Follow,
    FollowDeclaredNotRegistered,
    FollowRegisteredNotDeclared,
    FollowInputPort,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ready,
    NotReady,
    Retired,
    Failed,
    WrongSchema,
    ForeignCursor,
    Huge,
}

struct Log {
    head: u64,
    oldest: u64,
    mode: Mode,
}

/// One constructed component's own output state, read by its providers.
struct Shared {
    identity: GraphProducerIdentity,
    log: Mutex<Log>,
    blocked: AtomicBool,
    entered: Notify,
    unblock: Notify,
    active: AtomicUsize,
    hub: Mutex<OutputFollowHub>,
}

struct Active<'a>(&'a Shared);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn out() -> PortId {
    PortId::try_new("out").unwrap()
}

fn producer_for(component: &str) -> GraphProducerIdentity {
    GraphProducerIdentity::new(
        "scope".into(),
        "graph".into(),
        ComponentId::try_new(component).unwrap(),
        StreamId::try_new(format!("{component}/out")).unwrap(),
        true,
    )
    .unwrap()
}

fn record(index: u64) -> Record {
    GraphChangeCodec::element_record(&ElementImage::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", &index.to_string()),
            labels: Arc::from(Vec::<Arc<str>>::new()),
            effective_from: index,
        },
        properties: BTreeMap::new(),
    })
    .unwrap()
}

fn logical_envelope(identity: &GraphProducerIdentity, logical: u64) -> ChangeEnvelope {
    // The helper's per-component stream is the producer's stream.
    let mut envelope = super::envelope(identity.component_id(), 1000 + logical);
    GraphProducerProgress::annotate(&mut envelope, identity, logical).unwrap();
    envelope
}

impl Shared {
    fn new(component: &ComponentId) -> Arc<Self> {
        Arc::new(Self {
            identity: producer_for(component.as_str()),
            log: Mutex::new(Log {
                head: 0,
                oldest: 1,
                mode: Mode::Ready,
            }),
            blocked: AtomicBool::new(false),
            entered: Notify::new(),
            unblock: Notify::new(),
            active: AtomicUsize::new(0),
            hub: Mutex::new(
                OutputFollowHub::new(
                    NonZeroUsize::new(HUB_LEASES).unwrap(),
                    follow_limits(64, Duration::from_secs(60)),
                )
                .unwrap(),
            ),
        })
    }

    /// Commit logical outputs up to `head` and publish each to open leases.
    fn publish_to(&self, head: u64) {
        let from = self.log.lock().unwrap().head;
        for logical in from + 1..=head {
            self.log.lock().unwrap().head = logical;
            self.hub
                .lock()
                .unwrap()
                .publish(&logical_envelope(&self.identity, logical))
                .unwrap();
        }
    }

    fn cursor(&self, sequence: u64) -> OutputCursor {
        OutputCursor::try_new(self.identity.clone(), sequence).unwrap()
    }

    fn commit(&self, head: u64) {
        self.log.lock().unwrap().head = head;
    }

    fn retain_from(&self, oldest: u64) {
        self.log.lock().unwrap().oldest = oldest;
    }

    fn mode(&self, mode: Mode) {
        self.log.lock().unwrap().mode = mode;
    }

    fn block(&self, blocked: bool) {
        self.blocked.store(blocked, Ordering::SeqCst);
        if !blocked {
            self.unblock.notify_waiters();
        }
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    async fn enter(&self) -> Active<'_> {
        self.active.fetch_add(1, Ordering::SeqCst);
        let guard = Active(self);
        if self.blocked.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.unblock.notified().await;
        }
        guard
    }

    fn state(&self) -> (u64, u64, Mode) {
        let log = self.log.lock().unwrap();
        (log.head, log.oldest, log.mode)
    }

    fn mode_error(&self, mode: Mode) -> Option<OutputReadError> {
        match mode {
            Mode::NotReady => Some(OutputReadError::NotReady("warming".into())),
            Mode::Retired => Some(OutputReadError::Retired),
            Mode::Failed => Some(OutputReadError::Failed(anyhow::anyhow!("boom"))),
            _ => None,
        }
    }
}

struct Provider(Arc<Shared>);

impl Provider {
    fn build_snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        let (head, _, mode) = self.0.state();
        if let Some(error) = self.0.mode_error(mode) {
            return Err(error);
        }
        let descriptor = GraphChangeCodec::schema().descriptor().clone();
        match mode {
            Mode::WrongSchema => {
                let other = SchemaDescriptor::try_new(
                    SchemaId::try_new("other.schema").unwrap(),
                    SchemaVersion::try_new(1).unwrap(),
                    "none",
                    bytes::Bytes::from_static(b"other"),
                )
                .unwrap();
                OutputSnapshot::try_new(self.0.cursor(head), DATASET, other, Default::default())
            }
            Mode::ForeignCursor => OutputSnapshot::try_new(
                OutputCursor::try_new(producer_for("someone-else"), head).unwrap(),
                DATASET,
                descriptor,
                Default::default(),
            ),
            _ => {
                let count = if mode == Mode::Huge {
                    abi::output_reads::MAX_SNAPSHOT_RECORDS as u64 + 1
                } else {
                    head
                };
                let records: Vec<_> = (1..=count)
                    .map(|index| {
                        let record = record(index);
                        (record.identity().clone(), record)
                    })
                    .collect();
                OutputSnapshot::try_new(
                    self.0.cursor(head),
                    DATASET,
                    descriptor,
                    records.into_iter().collect(),
                )
            }
        }
    }
}

#[async_trait]
impl SnapshotProvider for Provider {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        let _active = self.0.enter().await;
        self.build_snapshot()
    }
}

#[async_trait]
impl SnapshotFollowProvider for Provider {
    async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> Result<OutputSnapshotFollow, OutputReadError> {
        let _active = self.0.enter().await;
        // The snapshot and the tail are captured under one serialization point.
        let mut hub = self.0.hub.lock().unwrap();
        hub.open(self.build_snapshot()?, limits)
    }
}

#[async_trait]
impl ReplayProvider for Provider {
    async fn committed_head(&self) -> Result<OutputCursor, OutputReadError> {
        let _active = self.0.enter().await;
        let (head, _, mode) = self.0.state();
        if let Some(error) = self.0.mode_error(mode) {
            return Err(error);
        }
        if mode == Mode::ForeignCursor {
            return Ok(OutputCursor::try_new(producer_for("someone-else"), head).unwrap());
        }
        Ok(self.0.cursor(head))
    }

    async fn replay_after(&self, after: &OutputCursor) -> Result<OutputReplay, OutputReadError> {
        let _active = self.0.enter().await;
        let (head, oldest, mode) = self.0.state();
        if let Some(error) = self.0.mode_error(mode) {
            return Err(error);
        }
        let current = self.0.cursor(head);
        after.ensure_same_epoch(&current)?;
        if after.sequence() > head || after.sequence() + 1 < oldest {
            return Err(OutputReadError::Gap {
                requested: after.sequence(),
                oldest: Some(oldest),
                latest: head,
            });
        }
        let envelopes = (after.sequence() + 1..=head)
            .map(|logical| logical_envelope(&self.0.identity, logical))
            .collect();
        OutputReplay::try_new(after.clone(), current, envelopes)
    }
}

struct Node {
    descriptor: ComponentDescriptor,
    shared: Arc<Shared>,
    kind: Kind,
}

#[async_trait]
impl sdk::NativeComponent for Node {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn output_readers(&self) -> OutputReaders {
        let provider = Arc::new(Provider(self.shared.clone()));
        let readers = OutputReaders::new();
        match self.kind {
            Kind::Both | Kind::Transformer => readers
                .with_snapshot(out(), provider.clone())
                .with_replay(out(), provider),
            Kind::SnapshotOnly | Kind::RegisteredNotDeclared => {
                readers.with_snapshot(out(), provider)
            }
            Kind::ReplayOnly => readers.with_replay(out(), provider),
            Kind::Follow => readers
                .with_snapshot(out(), provider.clone())
                .with_replay(out(), provider.clone())
                .with_snapshot_follow(out(), provider),
            Kind::FollowRegisteredNotDeclared => readers.with_snapshot_follow(out(), provider),
            _ => readers,
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
impl sdk::NativeSource for Node {
    /// Never produces: pending data work must not delay output reads.
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

#[async_trait]
impl sdk::NativeTransformer for Node {
    async fn transform(&mut self, _input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}

type Created = Arc<Mutex<BTreeMap<String, Arc<Shared>>>>;

struct Factory {
    kind: Kind,
    created: Created,
}

impl Factory {
    fn transformer(&self) -> bool {
        matches!(
            self.kind,
            Kind::Transformer | Kind::InputPort | Kind::FollowInputPort
        )
    }
}

impl sdk::Factory for Factory {
    fn metadata(&self) -> FactoryMetadata {
        let ports = if self.transformer() {
            vec![port(PortDirection::Input), port(PortDirection::Output)]
        } else {
            vec![port(PortDirection::Output)]
        };
        let (output_snapshots, output_replays) = match self.kind {
            Kind::Both | Kind::Transformer | Kind::Follow => (vec![out()], vec![out()]),
            Kind::SnapshotOnly | Kind::DeclaredNotRegistered => (vec![out()], vec![]),
            Kind::ReplayOnly => (vec![], vec![out()]),
            Kind::RegisteredNotDeclared
            | Kind::FollowDeclaredNotRegistered
            | Kind::FollowRegisteredNotDeclared
            | Kind::FollowInputPort => (vec![], vec![]),
            Kind::UnknownPort => (vec![PortId::try_new("missing").unwrap()], vec![]),
            Kind::DuplicatePort => (vec![out(), out()], vec![]),
            Kind::InputPort => (vec![PortId::try_new("in").unwrap()], vec![]),
        };
        let output_follows = match self.kind {
            Kind::Follow | Kind::FollowDeclaredNotRegistered => vec![out()],
            Kind::FollowInputPort => vec![PortId::try_new("in").unwrap()],
            _ => vec![],
        };
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/output-reads", "1").unwrap(),
            role: if self.transformer() {
                ComponentRole::Transformer
            } else {
                ComponentRole::Source
            },
            configuration_version: 1,
            configuration: ConfigSchema {
                fields: BTreeMap::new(),
                allow_additional: true,
            },
            ports,
            completion: None,
            capabilities: Capabilities {
                output_snapshots,
                output_replays,
                output_follows,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let shared = Shared::new(&request.id);
        self.created
            .lock()
            .unwrap()
            .insert(request.id.to_string(), shared.clone());
        let node = Node {
            descriptor: ComponentDescriptor::try_new(
                request.id.clone(),
                sdk::Factory::metadata(self).ports,
            )?,
            shared,
            kind: self.kind,
        };
        Ok(sdk::CreatedComponent {
            component: if self.transformer() {
                sdk::Component::Transformer(Box::new(node))
            } else {
                sdk::Component::Source(Box::new(node))
            },
            control_handler: None,
        })
    }
}

struct Fixture {
    plugin: Arc<NativePlugin>,
    created: Created,
}

fn definition(kind: Kind, created: &Created) -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "native-output-reads-test",
        "1",
        vec![Arc::new(Factory {
            kind,
            created: created.clone(),
        })],
        vec![GraphChangeCodec::schema()],
    )
}

fn fixture(kind: Kind) -> anyhow::Result<Fixture> {
    let created = Created::default();
    Ok(Fixture {
        plugin: NativePlugin::from_definition(definition(kind, &created)?)?,
        created,
    })
}

impl Fixture {
    fn create(&self, name: &str) -> anyhow::Result<(NativeComponentProxy, Arc<Shared>)> {
        let proxy =
            self.plugin.factories()[0].create_component(ComponentId::try_new(name)?, json!({}))?;
        let shared = self.created.lock().unwrap()[name].clone();
        Ok((proxy, shared))
    }
}

fn snapshots(readers: &OutputReaders) -> Arc<dyn SnapshotProvider> {
    readers.ports()[&out()]
        .snapshot()
        .cloned()
        .expect("snapshot provider")
}

fn replays(readers: &OutputReaders) -> Arc<dyn ReplayProvider> {
    readers.ports()[&out()]
        .replay()
        .cloned()
        .expect("replay provider")
}

fn logical(envelope: &ChangeEnvelope) -> u64 {
    GraphProducerProgress::from_envelope(envelope)
        .unwrap()
        .expect("logical producer progress")
        .sequence()
}

#[tokio::test(flavor = "current_thread")]
async fn source_snapshot_and_replay_cross_the_exported_tables() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Both)?;
    let (mut proxy, shared) = fixture.create("source")?;
    shared.commit(3);
    proxy.start().await?;
    let readers = proxy.output_readers();

    let snapshot = snapshots(&readers).snapshot().await?;
    assert_eq!(snapshot.cursor(), &shared.cursor(3));
    assert_eq!(snapshot.dataset(), DATASET);
    assert_eq!(snapshot.schema(), GraphChangeCodec::schema().descriptor());
    assert_eq!(snapshot.records().len(), 3);
    for index in 1..=3 {
        let expected = record(index);
        assert_eq!(snapshot.records().get(expected.identity()), Some(&expected));
    }

    let replay = replays(&readers);
    let head = replay.committed_head().await?;
    assert_eq!(head, shared.cursor(3));
    let all = replay.replay_after(&head.at_sequence(0)).await?;
    assert_eq!(all.after().sequence(), 0);
    assert_eq!(all.head(), &head);
    assert_eq!(
        all.envelopes().iter().map(logical).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    // Replay follows logical producer progress; transport sequences are separate.
    assert_eq!(all.envelopes()[0].system().sequence(), 1001);
    let tail = replay.replay_after(&head.at_sequence(2)).await?;
    assert_eq!(
        tail.envelopes().iter().map(logical).collect::<Vec<_>>(),
        [3]
    );
    assert!(replay.replay_after(&head).await?.envelopes().is_empty());
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_and_replay_capabilities_are_independent_per_output() -> anyhow::Result<()> {
    for (kind, snapshot, replay) in [
        (Kind::SnapshotOnly, true, false),
        (Kind::ReplayOnly, false, true),
        (Kind::Both, true, true),
        (Kind::Transformer, true, true),
    ] {
        let fixture = fixture(kind)?;
        let capabilities = &fixture.plugin.factories()[0].metadata().capabilities;
        assert_eq!(!capabilities.output_snapshots.is_empty(), snapshot);
        assert_eq!(!capabilities.output_replays.is_empty(), replay);
        let (proxy, _) = fixture.create("component")?;
        let readers = proxy.output_readers();
        let registered = &readers.ports()[&out()];
        assert_eq!(registered.snapshot().is_some(), snapshot, "{kind:?}");
        assert_eq!(registered.replay().is_some(), replay, "{kind:?}");
        assert_eq!(readers.ports().len(), 1);
    }
    let fixture = fixture(Kind::Transformer)?;
    let (mut proxy, shared) = fixture.create("transformer")?;
    shared.commit(2);
    proxy.start().await?;
    let readers = proxy.output_readers();
    assert_eq!(snapshots(&readers).snapshot().await?.records().len(), 2);
    assert_eq!(replays(&readers).committed_head().await?.sequence(), 2);
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn declarations_must_match_registrations_and_declared_outputs() -> anyhow::Result<()> {
    for kind in [Kind::DeclaredNotRegistered, Kind::RegisteredNotDeclared] {
        assert!(
            fixture(kind)?.create("mismatch").is_err(),
            "{kind:?} created a component"
        );
    }
    for kind in [Kind::UnknownPort, Kind::DuplicatePort, Kind::InputPort] {
        assert!(
            definition(kind, &Created::default()).is_err(),
            "{kind:?} metadata was accepted"
        );
    }
    Ok(())
}

fn raw_open(
    table: &abi::output_reads::PluginOutputReadsV1,
    proxy: &NativeComponentProxy,
    port: &str,
    capability: u32,
) -> Result<abi::output_reads::OutputReadHandle, sdk::transport::Failure> {
    let mut handle = abi::output_reads::OutputReadHandle::null();
    unsafe {
        sdk::transport::take_status((table.open.unwrap())(
            proxy.inner.state(),
            abi::BorrowedBytes::new(port.as_bytes()),
            capability,
            &mut handle,
        ))?;
    }
    Ok(handle)
}

fn raw_request(
    handle: &abi::output_reads::OutputReadHandle,
    code: u32,
    input: &[u8],
) -> Result<sdk::transport::OperationFuture, sdk::transport::Failure> {
    let mut operation = abi::OperationHandle::null();
    unsafe {
        sdk::transport::take_status(((*handle.vtable).request.unwrap())(
            handle.state,
            code,
            abi::BorrowedBytes::new(input),
            &mut operation,
        ))?;
        sdk::transport::OperationFuture::new(operation)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn raw_extension_rejects_unknown_ports_capabilities_and_operations() -> anyhow::Result<()> {
    use abi::output_reads::{capability, operation};
    let created = Created::default();
    let table = unsafe {
        *sdk::ExportedPlugin::new(definition(Kind::SnapshotOnly, &created)?)?.output_reads()
    };
    let fixture = fixture(Kind::SnapshotOnly)?;
    let (proxy, _) = fixture.create("raw")?;

    let code =
        |result: Result<abi::output_reads::OutputReadHandle, sdk::transport::Failure>| match result
        {
            Ok(_) => panic!("open succeeded"),
            Err(error) => error.code,
        };
    assert_eq!(
        code(raw_open(&table, &proxy, "missing", capability::SNAPSHOT)),
        abi::status::INVALID_ARGUMENT
    );
    assert_eq!(
        code(raw_open(&table, &proxy, "out", 99)),
        abi::status::INVALID_ARGUMENT
    );
    assert_eq!(
        code(raw_open(&table, &proxy, "out", capability::REPLAY)),
        abi::status::UNSUPPORTED,
        "an output without a replay provider cannot open one"
    );
    assert_eq!(
        code(raw_open(&table, &proxy, "\u{0}\u{1}", capability::SNAPSHOT)),
        abi::status::INVALID_ARGUMENT
    );

    let handle = raw_open(&table, &proxy, "out", capability::SNAPSHOT)?;
    let status = |result: Result<sdk::transport::OperationFuture, sdk::transport::Failure>| {
        result.err().expect("request was accepted").code
    };
    assert_eq!(
        status(raw_request(&handle, operation::COMMITTED_HEAD, &[])),
        abi::status::UNSUPPORTED
    );
    assert_eq!(
        status(raw_request(&handle, operation::REPLAY_AFTER, &[1])),
        abi::status::UNSUPPORTED
    );
    assert_eq!(
        status(raw_request(&handle, operation::SNAPSHOT, &[1])),
        abi::status::INVALID_ARGUMENT
    );
    assert_eq!(
        status(raw_request(
            &handle,
            operation::SNAPSHOT,
            &vec![0u8; abi::output_reads::MAX_REQUEST_BYTES + 1]
        )),
        abi::status::PROTOCOL
    );
    // An inactive component reports not-active, never an empty snapshot.
    let inactive = raw_request(&handle, operation::SNAPSHOT, &[])
        .err()
        .map(|e| e.code);
    assert_eq!(inactive, Some(abi::output_reads::status::NOT_ACTIVE));
    unsafe { ((*handle.vtable).release.unwrap())(handle.state) };
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn reads_are_independent_of_pending_data_work_and_retire_with_the_proxy() -> anyhow::Result<()>
{
    let fixture = fixture(Kind::Both)?;
    let (mut proxy, shared) = fixture.create("pending")?;
    shared.commit(2);
    proxy.start().await?;
    let readers = proxy.output_readers();
    let snapshot = snapshots(&readers);
    let replay = replays(&readers);

    let pending = tokio::spawn(async move {
        let result = proxy.next().await;
        (proxy, result)
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!pending.is_finished(), "source next must still be pending");
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        let snapshot = snapshot.snapshot().await?;
        let tail = replay
            .replay_after(&snapshot.cursor().at_sequence(0))
            .await?;
        anyhow::Ok((snapshot.records().len(), tail.envelopes().len()))
    })
    .await
    .expect("output read waited behind pending Source::next")?;
    assert_eq!(read, (2, 2));
    assert!(!pending.is_finished());

    // Dropping the proxy retires its outstanding providers.
    pending.abort();
    let _ = pending.await;
    assert!(matches!(
        snapshot.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    assert!(matches!(
        replay.committed_head().await,
        Err(OutputReadError::Retired)
    ));
    drop((readers, snapshot, replay));
    assert_eq!(
        Arc::strong_count(&shared),
        2,
        "provider or component state outlived the released proxy"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn typed_errors_are_preserved_and_never_become_empty_results() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Both)?;
    let (mut proxy, shared) = fixture.create("errors")?;
    proxy.start().await?;
    let readers = proxy.output_readers();
    let (snapshot, replay) = (snapshots(&readers), replays(&readers));

    shared.commit(5);
    shared.retain_from(3);
    assert!(matches!(
        replay.replay_after(&shared.cursor(0)).await,
        Err(OutputReadError::Gap {
            requested: 0,
            oldest: Some(3),
            latest: 5
        })
    ));
    assert!(matches!(
        replay.replay_after(&shared.cursor(9)).await,
        Err(OutputReadError::Gap { requested: 9, .. })
    ));
    assert_eq!(
        replay
            .replay_after(&shared.cursor(2))
            .await?
            .envelopes()
            .len(),
        3
    );

    let foreign = OutputCursor::try_new(producer_for("errors"), 4)?;
    let Err(OutputReadError::EpochChanged { requested, current }) =
        replay.replay_after(&foreign).await
    else {
        panic!("a cursor from another epoch must report an epoch change")
    };
    assert_eq!(&*requested, foreign.producer());
    assert_eq!(&*current, &shared.identity);

    shared.mode(Mode::NotReady);
    assert!(
        matches!(snapshot.snapshot().await, Err(OutputReadError::NotReady(message)) if message == "warming")
    );
    assert!(matches!(
        replay.committed_head().await,
        Err(OutputReadError::NotReady(_))
    ));
    shared.mode(Mode::Retired);
    assert!(matches!(
        snapshot.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    shared.mode(Mode::Failed);
    let Err(OutputReadError::Failed(error)) = snapshot.snapshot().await else {
        panic!("a provider failure must stay a failure")
    };
    assert!(error.to_string().contains("boom"));
    for mode in [Mode::WrongSchema, Mode::ForeignCursor] {
        shared.mode(mode);
        assert!(
            matches!(snapshot.snapshot().await, Err(OutputReadError::Invalid(_))),
            "contract violation was not reported as invalid"
        );
    }
    shared.mode(Mode::ForeignCursor);
    assert!(matches!(
        replay.committed_head().await,
        Err(OutputReadError::Invalid(_))
    ));
    shared.mode(Mode::Ready);
    assert_eq!(snapshot.snapshot().await?.records().len(), 5);
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn inactive_components_are_not_ready_and_stop_ends_in_flight_reads() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Both)?;
    let (mut proxy, shared) = fixture.create("lifecycle")?;
    shared.commit(1);
    let readers = proxy.output_readers();
    let snapshot = snapshots(&readers);
    assert!(
        matches!(snapshot.snapshot().await, Err(OutputReadError::NotReady(_))),
        "a read before START must not answer"
    );
    proxy.start().await?;
    assert_eq!(snapshot.snapshot().await?.records().len(), 1);

    shared.block(true);
    let read = tokio::spawn({
        let snapshot = snapshot.clone();
        async move { snapshot.snapshot().await }
    });
    shared.entered.notified().await;
    assert_eq!(shared.active(), 1);
    proxy.stop().await?;
    assert!(matches!(read.await?, Err(OutputReadError::Retired)));
    assert_eq!(
        shared.active(),
        0,
        "the in-flight provider read was not cancelled"
    );
    shared.block(false);
    assert!(
        matches!(snapshot.snapshot().await, Err(OutputReadError::NotReady(_))),
        "a stopped component must not answer"
    );

    proxy.start().await?;
    assert_eq!(snapshot.snapshot().await?.records().len(), 1);
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_reads_release_their_slots_and_cancel_the_provider() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Both)?;
    let (mut proxy, shared) = fixture.create("cancelled")?;
    shared.commit(1);
    proxy.start().await?;
    let readers = proxy.output_readers();
    let snapshot = snapshots(&readers);

    shared.block(true);
    for _ in 0..(abi::output_reads::MAX_CONCURRENT_READS * 3) {
        let result = tokio::time::timeout(Duration::from_millis(10), snapshot.snapshot()).await;
        assert!(result.is_err(), "blocked read unexpectedly completed");
    }
    assert_eq!(
        shared.active(),
        0,
        "cancelled provider futures were retained"
    );
    shared.block(false);
    assert_eq!(
        snapshot.snapshot().await?.records().len(),
        1,
        "cancelled reads leaked concurrency slots"
    );
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_answers_are_explicit_limit_errors() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Both)?;
    let (mut proxy, shared) = fixture.create("limits")?;
    proxy.start().await?;
    let readers = proxy.output_readers();

    shared.mode(Mode::Huge);
    let Err(OutputReadError::Failed(error)) = snapshots(&readers).snapshot().await else {
        panic!("an oversized snapshot must be an explicit limit error")
    };
    assert!(error
        .downcast_ref::<sdk::output_reads::OutputReadLimit>()
        .is_some());

    shared.mode(Mode::Ready);
    shared.commit(abi::output_reads::MAX_REPLAY_ENVELOPES as u64 + 1);
    let replay = replays(&readers);
    let Err(OutputReadError::Failed(error)) = replay.replay_after(&shared.cursor(0)).await else {
        panic!("an oversized replay must be an explicit limit error")
    };
    assert!(error
        .downcast_ref::<sdk::output_reads::OutputReadLimit>()
        .is_some());
    let within = shared.cursor(abi::output_reads::MAX_REPLAY_ENVELOPES as u64);
    assert_eq!(
        replay
            .replay_after(&shared.cursor(1))
            .await?
            .envelopes()
            .len(),
        abi::output_reads::MAX_REPLAY_ENVELOPES
    );
    assert_eq!(replay.replay_after(&within).await?.envelopes().len(), 1);
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_replaced_component_never_serves_its_successor_or_predecessor() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Both)?;
    let (mut first, first_state) = fixture.create("replaced")?;
    first_state.commit(1);
    first.start().await?;
    let old = snapshots(&first.output_readers());
    first.stop().await?;
    drop(first);

    let (mut second, second_state) = fixture.create("replaced-again")?;
    second_state.commit(2);
    second.start().await?;
    let new = snapshots(&second.output_readers());
    assert!(matches!(
        old.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    let snapshot = new.snapshot().await?;
    assert_eq!(snapshot.records().len(), 2);
    assert_eq!(snapshot.cursor().producer(), &second_state.identity);
    second.stop().await?;
    Ok(())
}

fn follow_provider(readers: &OutputReaders) -> Arc<dyn SnapshotFollowProvider> {
    readers.ports()[&out()]
        .snapshot_follow()
        .cloned()
        .expect("snapshot-follow provider")
}

fn open_limits() -> OutputFollowLimits {
    follow_limits(8, Duration::from_secs(30))
}

#[tokio::test(flavor = "current_thread")]
async fn follow_delivers_a_coherent_snapshot_then_the_live_tail_in_order() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Follow)?;
    let (mut proxy, shared) = fixture.create("follow")?;
    shared.commit(2);
    proxy.start().await?;
    let readers = proxy.output_readers();
    let follow = follow_provider(&readers)
        .snapshot_and_follow(open_limits())
        .await?;
    assert_eq!(follow.snapshot().cursor(), &shared.cursor(2));
    assert_eq!(follow.snapshot().records().len(), 2);
    // Output committed while the snapshot is being consumed is retained.
    shared.publish_to(4);
    let (snapshot, mut tail) = follow.into_parts();
    assert_eq!(snapshot.records().len(), 2);
    for expected in [3, 4] {
        let envelope = tail.next().await?;
        assert_eq!(logical(&envelope), expected);
        assert_eq!(envelope.system().sequence(), 1000 + expected);
    }
    // A cancelled next consumes nothing.
    assert!(tokio::time::timeout(Duration::from_millis(30), tail.next())
        .await
        .is_err());
    shared.publish_to(5);
    assert_eq!(logical(&tail.next().await?), 5);
    drop(tail);
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn follow_lag_expiry_and_capacity_are_distinct_typed_errors() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Follow)?;
    let (mut proxy, shared) = fixture.create("follow-errors")?;
    proxy.start().await?;
    let readers = proxy.output_readers();
    let provider = follow_provider(&readers);

    // Per-tail capacity overflow is a gap, never an expiry, and it is sticky.
    let (_, mut lagging) = provider
        .snapshot_and_follow(follow_limits(2, Duration::from_secs(30)))
        .await?
        .into_parts();
    shared.publish_to(3);
    for _ in 0..2 {
        assert!(matches!(
            lagging.next().await,
            Err(OutputReadError::Gap { .. })
        ));
    }

    // Expiry wakes a pending next.
    let (_, mut short) = provider
        .snapshot_and_follow(follow_limits(8, Duration::from_millis(150)))
        .await?
        .into_parts();
    for _ in 0..2 {
        let result = tokio::time::timeout(Duration::from_secs(5), short.next())
            .await
            .expect("expiry must end a pending next");
        assert!(matches!(result, Err(OutputReadError::LeaseExpired)));
    }

    // Capacity is explicit and returned when a lease is released.
    let mut held = Vec::new();
    for _ in 0..HUB_LEASES {
        held.push(provider.snapshot_and_follow(open_limits()).await?);
    }
    assert!(matches!(
        provider.snapshot_and_follow(open_limits()).await,
        Err(OutputReadError::NotReady(_))
    ));
    held.pop();
    provider.snapshot_and_follow(open_limits()).await?;
    held.clear();

    // Requests beyond the provider ceiling or with no lifetime are invalid.
    assert!(matches!(
        provider
            .snapshot_and_follow(follow_limits(65, Duration::from_secs(1)))
            .await,
        Err(OutputReadError::Invalid(_))
    ));
    assert!(matches!(
        provider
            .snapshot_and_follow(follow_limits(1, Duration::ZERO))
            .await,
        Err(OutputReadError::Invalid(_))
    ));

    // An oversized snapshot is an explicit limit and keeps no lease.
    shared.mode(Mode::Huge);
    let Err(OutputReadError::Failed(error)) = provider.snapshot_and_follow(open_limits()).await
    else {
        panic!("an oversized follow snapshot must be an explicit limit error")
    };
    assert!(error
        .downcast_ref::<sdk::output_reads::OutputReadLimit>()
        .is_some());
    shared.mode(Mode::Ready);
    for _ in 0..HUB_LEASES {
        held.push(provider.snapshot_and_follow(open_limits()).await?);
    }
    held.clear();
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stop_releases_leases_and_a_restart_never_revives_them() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Follow)?;
    let (mut proxy, shared) = fixture.create("follow-lifecycle")?;
    proxy.start().await?;
    let readers = proxy.output_readers();
    let provider = follow_provider(&readers);

    // A pending next ends with the activation.
    let (_, tail) = provider
        .snapshot_and_follow(open_limits())
        .await?
        .into_parts();
    let pending = tokio::spawn(async move {
        let mut tail = tail;
        tail.next().await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!pending.is_finished());
    proxy.stop().await?;
    assert!(matches!(pending.await?, Err(OutputReadError::Retired)));

    proxy.start().await?;
    let (_, mut old) = provider
        .snapshot_and_follow(open_limits())
        .await?
        .into_parts();
    proxy.stop().await?;
    // The host still holds the lease handle, but stop already released the
    // provider's retention, and a restart does not revive it.
    assert!(matches!(old.next().await, Err(OutputReadError::Retired)));
    proxy.start().await?;
    assert!(matches!(old.next().await, Err(OutputReadError::Retired)));
    let mut held = Vec::new();
    for _ in 0..HUB_LEASES {
        held.push(provider.snapshot_and_follow(open_limits()).await?);
    }
    shared.publish_to(1);
    let (_, mut current) = held.pop().unwrap().into_parts();
    shared.publish_to(2);
    // The new lease opened before either commit and sees both, in order.
    assert_eq!(logical(&current.next().await?), 1);
    assert_eq!(logical(&current.next().await?), 2);
    drop((old, held, current));
    proxy.stop().await?;
    drop(readers);
    drop(proxy);
    // The provider handle stays valid after release, but fails closed.
    assert!(matches!(
        provider.snapshot_and_follow(open_limits()).await,
        Err(OutputReadError::Retired)
    ));
    drop(provider);
    assert_eq!(
        Arc::strong_count(&shared),
        2,
        "a lease or provider outlived the released proxy"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_pending_follow_does_not_block_other_output_reads() -> anyhow::Result<()> {
    let fixture = fixture(Kind::Follow)?;
    let (mut proxy, shared) = fixture.create("follow-independent")?;
    shared.commit(1);
    proxy.start().await?;
    let readers = proxy.output_readers();
    let (_, tail) = follow_provider(&readers)
        .snapshot_and_follow(open_limits())
        .await?
        .into_parts();
    let pending = tokio::spawn(async move {
        let mut tail = tail;
        tail.next().await.map(|envelope| logical(&envelope))
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!pending.is_finished());
    tokio::time::timeout(Duration::from_secs(5), async {
        assert_eq!(snapshots(&readers).snapshot().await?.records().len(), 1);
        assert_eq!(replays(&readers).committed_head().await?.sequence(), 1);
        follow_provider(&readers)
            .snapshot_and_follow(open_limits())
            .await?;
        anyhow::Ok(())
    })
    .await
    .expect("point reads or another open waited behind a pending follow")?;
    shared.publish_to(2);
    assert_eq!(pending.await??, 2);
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn follow_declarations_must_match_registrations_and_outputs() -> anyhow::Result<()> {
    for kind in [
        Kind::FollowDeclaredNotRegistered,
        Kind::FollowRegisteredNotDeclared,
    ] {
        assert!(
            fixture(kind)?.create("mismatch").is_err(),
            "{kind:?} created a component"
        );
    }
    assert!(
        definition(Kind::FollowInputPort, &Created::default()).is_err(),
        "a follow declared on an input port was accepted"
    );
    let fixture = fixture(Kind::Follow)?;
    let capabilities = &fixture.plugin.factories()[0].metadata().capabilities;
    assert_eq!(capabilities.output_follows, vec![out()]);
    Ok(())
}

fn raw_next(
    lease: &abi::output_follow::OutputFollowLeaseHandle,
    acknowledged: u64,
) -> Result<sdk::transport::OperationFuture, sdk::transport::Failure> {
    let input = sdk::wire::encode(&sdk::output_reads::WireNext { acknowledged }).unwrap();
    let mut operation = abi::OperationHandle::null();
    unsafe {
        sdk::transport::take_status(((*lease.vtable).next.unwrap())(
            lease.state,
            abi::BorrowedBytes::new(&input),
            &mut operation,
        ))?;
        sdk::transport::OperationFuture::new(operation)
    }
}

async fn raw_item(
    lease: &abi::output_follow::OutputFollowLeaseHandle,
    acknowledged: u64,
) -> anyhow::Result<(u64, Vec<u8>)> {
    let bytes = raw_next(lease, acknowledged)?.await?;
    match sdk::wire::decode::<sdk::output_reads::Outcome<sdk::output_reads::WireFollowItem<'_>>>(
        &bytes,
    )? {
        sdk::output_reads::Outcome::Ok(item) => Ok((item.index, item.envelope.into_owned())),
        other => anyhow::bail!("unexpected follow outcome {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn raw_lease_retains_an_unacknowledged_item_until_it_is_observed() -> anyhow::Result<()> {
    use abi::output_follow as follow;
    let created = Created::default();
    let table =
        unsafe { *sdk::ExportedPlugin::new(definition(Kind::Follow, &created)?)?.output_follow() };
    let fixture = fixture(Kind::Follow)?;
    let (mut proxy, shared) = fixture.create("raw-follow")?;

    let mut provider = follow::OutputFollowProviderHandle::null();
    let status = |result: Result<(), sdk::transport::Failure>| result.err().map(|e| e.code);
    for (port, expected) in [
        ("missing", abi::status::INVALID_ARGUMENT),
        ("\u{0}\u{1}", abi::status::INVALID_ARGUMENT),
    ] {
        assert_eq!(
            status(unsafe {
                sdk::transport::take_status((table.open.unwrap())(
                    proxy.inner.state(),
                    abi::BorrowedBytes::new(port.as_bytes()),
                    &mut provider,
                ))
            }),
            Some(expected)
        );
    }
    unsafe {
        sdk::transport::take_status((table.open.unwrap())(
            proxy.inner.state(),
            abi::BorrowedBytes::new(b"out"),
            &mut provider,
        ))?;
    }
    let limits = sdk::wire::encode(&sdk::output_reads::WireFollowLimits {
        max_envelopes: 8,
        max_bytes: 1 << 20,
        lifetime_nanos: 30_000_000_000,
    })?;
    let begin = |limits: &[u8]| {
        let mut lease = follow::OutputFollowLeaseHandle::null();
        let mut operation = abi::OperationHandle::null();
        unsafe {
            sdk::transport::take_status(((*provider.vtable).begin.unwrap())(
                provider.state,
                abi::BorrowedBytes::new(limits),
                &mut lease,
                &mut operation,
            ))?;
            Ok::<_, sdk::transport::Failure>((
                lease,
                sdk::transport::OperationFuture::new(operation)?,
            ))
        }
    };
    // Before START the component is not active: no lease is created.
    assert_eq!(
        begin(&limits).err().map(|e| e.code),
        Some(abi::output_reads::status::NOT_ACTIVE)
    );
    proxy.start().await?;
    assert_eq!(
        begin(&[1, 2, 3]).err().map(|e| e.code),
        Some(abi::status::INVALID_ARGUMENT)
    );
    let (lease, opening) = begin(&limits)?;
    let reply = opening.await?;
    assert!(matches!(
        sdk::wire::decode::<sdk::output_reads::Outcome<sdk::output_reads::WireSnapshot<'_>>>(
            &reply
        )?,
        sdk::output_reads::Outcome::Ok(_)
    ));

    shared.publish_to(2);
    let first = raw_item(&lease, 0).await?;
    // The reply is considered lost: the same item is resent, not skipped.
    let resent = raw_item(&lease, 0).await?;
    assert_eq!(first, resent);
    assert_eq!(first.0, 1);
    let second = raw_item(&lease, 1).await?;
    assert_eq!(second.0, 2);
    let never_delivered = raw_next(&lease, 9)?.await.expect_err("rejected");
    assert_eq!(never_delivered.code, abi::status::INVALID_ARGUMENT);

    // One active next per lease; cancelling it releases the slot and loses nothing.
    let pending = raw_next(&lease, 2)?;
    assert_eq!(
        raw_next(&lease, 2).err().map(|e| e.code),
        Some(abi::status::BUSY)
    );
    drop(pending);
    shared.publish_to(3);
    assert_eq!(raw_item(&lease, 2).await?.0, 3);

    unsafe {
        ((*lease.vtable).release.unwrap())(lease.state);
        ((*provider.vtable).release.unwrap())(provider.state);
    }
    proxy.stop().await?;
    Ok(())
}

type RawLease = abi::output_follow::OutputFollowLeaseHandle;
type RawProvider = abi::output_follow::OutputFollowProviderHandle;

fn raw_follow_table() -> anyhow::Result<abi::output_follow::PluginOutputFollowV1> {
    Ok(unsafe {
        *sdk::ExportedPlugin::new(definition(Kind::Follow, &Created::default())?)?.output_follow()
    })
}

fn raw_follow_provider(
    table: &abi::output_follow::PluginOutputFollowV1,
    proxy: &NativeComponentProxy,
) -> anyhow::Result<RawProvider> {
    let mut provider = RawProvider::null();
    unsafe {
        sdk::transport::take_status((table.open.unwrap())(
            proxy.inner.state(),
            abi::BorrowedBytes::new(b"out"),
            &mut provider,
        ))?;
    }
    Ok(provider)
}

fn raw_limits() -> Vec<u8> {
    sdk::wire::encode(&sdk::output_reads::WireFollowLimits {
        max_envelopes: 8,
        max_bytes: 1 << 20,
        lifetime_nanos: 30_000_000_000,
    })
    .unwrap()
}

fn raw_follow_begin(
    provider: &RawProvider,
) -> Result<(RawLease, sdk::transport::OperationFuture), sdk::transport::Failure> {
    let limits = raw_limits();
    let mut lease = RawLease::null();
    let mut operation = abi::OperationHandle::null();
    unsafe {
        sdk::transport::take_status(((*provider.vtable).begin.unwrap())(
            provider.state,
            abi::BorrowedBytes::new(&limits),
            &mut lease,
            &mut operation,
        ))?;
        Ok((lease, sdk::transport::OperationFuture::new(operation)?))
    }
}

fn raw_release(lease: &RawLease) {
    unsafe { ((*lease.vtable).release.unwrap())(lease.state) };
}

/// Open a lease and report whether the provider granted it a snapshot.
async fn raw_open_lease(provider: &RawProvider) -> anyhow::Result<(RawLease, bool)> {
    let (lease, opening) = raw_follow_begin(provider)?;
    let bytes = opening.await?;
    let granted = matches!(
        sdk::wire::decode::<sdk::output_reads::Outcome<sdk::output_reads::WireSnapshot<'_>>>(
            &bytes
        )?,
        sdk::output_reads::Outcome::Ok(_)
    );
    Ok((lease, granted))
}

#[tokio::test(flavor = "current_thread")]
async fn releasing_a_lease_drops_a_running_next_while_its_operation_is_retained(
) -> anyhow::Result<()> {
    let table = raw_follow_table()?;
    let fixture = fixture(Kind::Follow)?;
    let (mut proxy, _shared) = fixture.create("release-next")?;
    proxy.start().await?;
    let provider = raw_follow_provider(&table, &proxy)?;

    let (first, granted) = raw_open_lease(&provider).await?;
    assert!(granted);
    // Poll the next once so the provider's tail is inside an in-progress step.
    let mut running = raw_next(&first, 0)?;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut running)
            .await
            .is_err()
    );
    let mut others = Vec::new();
    for _ in 1..HUB_LEASES {
        let (lease, granted) = raw_open_lease(&provider).await?;
        assert!(granted);
        others.push(lease);
    }
    let (full, granted) = raw_open_lease(&provider).await?;
    assert!(
        !granted,
        "the hub is at capacity while the first lease retains"
    );
    raw_release(&full);

    // Release while the operation handle is still retained and unpolled.
    raw_release(&first);
    let (fresh, granted) = raw_open_lease(&provider).await?;
    assert!(
        granted,
        "release did not synchronously drop the provider's retention"
    );
    let error = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the retained operation must end promptly")
        .expect_err("a released lease cannot deliver");
    assert_eq!(error.code, abi::status::CLOSED);

    for lease in others.iter().chain([&fresh]) {
        raw_release(lease);
    }
    unsafe { ((*provider.vtable).release.unwrap())(provider.state) };
    proxy.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn releasing_a_lease_cancels_its_pending_open_while_its_operation_is_retained(
) -> anyhow::Result<()> {
    let table = raw_follow_table()?;
    let fixture = fixture(Kind::Follow)?;
    let (mut proxy, shared) = fixture.create("release-open")?;
    proxy.start().await?;
    let provider = raw_follow_provider(&table, &proxy)?;

    shared.block(true);
    let (lease, mut opening) = raw_follow_begin(&provider)?;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut opening)
            .await
            .is_err()
    );
    assert_eq!(shared.active(), 1, "the provider open is in progress");
    raw_release(&lease);
    assert_eq!(
        shared.active(),
        0,
        "release did not synchronously drop the provider's open future"
    );
    let error = tokio::time::timeout(Duration::from_secs(5), opening)
        .await
        .expect("the retained open must end promptly")
        .expect_err("a released lease cannot open");
    assert_eq!(error.code, abi::status::CLOSED);
    shared.block(false);

    // Stop closes leases too, with operation handles still retained.
    let (stopped, granted) = raw_open_lease(&provider).await?;
    assert!(granted);
    let mut waiting = raw_next(&stopped, 0)?;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err()
    );
    proxy.stop().await?;
    let error = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("a stopped lease's operation must end promptly")
        .expect_err("a stopped lease cannot deliver");
    assert_eq!(error.code, abi::status::CLOSED);
    raw_release(&stopped);
    unsafe { ((*provider.vtable).release.unwrap())(provider.state) };
    Ok(())
}
