// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::interface::{IndexError, OutboxWriter};

struct PagedReadGate {
    after: std::sync::atomic::AtomicU64,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    fail: AtomicBool,
}

impl Default for PagedReadGate {
    fn default() -> Self {
        Self {
            after: std::sync::atomic::AtomicU64::new(u64::MAX),
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
            fail: AtomicBool::new(false),
        }
    }
}

struct PagedOutbox {
    inner: Arc<dyn OutboxWriter>,
    gate: Arc<PagedReadGate>,
    work: Arc<drasi_core::computation::ComputationIoScope>,
}

struct PagedCleanup {
    inner: Arc<dyn drasi_core::computation::ComputationResourceCleanup>,
    work: Arc<drasi_core::computation::ComputationIoScope>,
}

#[async_trait]
impl drasi_core::computation::ComputationResourceCleanup for PagedCleanup {
    fn cancel(&self) {
        self.work.cancel();
        self.inner.cancel();
    }
    async fn quiesce(&self) -> Result<(), IndexError> {
        self.work.quiesce().await?;
        self.inner.quiesce().await
    }
    async fn shutdown(&self) -> Result<(), IndexError> {
        self.work.shutdown().await?;
        self.inner.shutdown().await
    }
}

#[async_trait]
impl OutboxWriter for PagedOutbox {
    async fn append(&self, key: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        self.inner.append(key, sequence, data).await
    }
    async fn read_from(&self, key: &str, after: u64) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        assert!(
            !key.ends_with("_6576656e7473"),
            "paged journals must not eagerly load event history"
        );
        self.inner.read_from(key, after).await
    }
    async fn read_page(
        &self,
        key: &str,
        after: u64,
        limits: drasi_core::interface::OutboxPageLimits,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let inner = self.inner.clone();
        let gate = self.gate.clone();
        let key = key.to_owned();
        self.work
            .run_async(async move {
                let page = inner.read_page(&key, after, limits).await?;
                if gate
                    .after
                    .compare_exchange(after, u64::MAX, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    gate.entered.notify_one();
                    gate.resume.notified().await;
                    if gate.fail.load(Ordering::SeqCst) {
                        return Err(IndexError::IOError);
                    }
                }
                Ok(page)
            })
            .await
    }
    async fn read_latest_sequence(&self, key: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(key).await
    }
    async fn clear(&self, key: &str) -> Result<(), IndexError> {
        self.inner.clear(key).await
    }
    async fn append_and_trim(
        &self,
        key: &str,
        sequence: u64,
        data: &[u8],
        floor: u64,
    ) -> Result<usize, IndexError> {
        self.inner.append_and_trim(key, sequence, data, floor).await
    }
    async fn trim_before(&self, key: &str, floor: u64) -> Result<usize, IndexError> {
        self.inner.trim_before(key, floor).await
    }
    async fn trim_to_capacity(&self, key: &str, capacity: usize) -> Result<usize, IndexError> {
        self.inner.trim_to_capacity(key, capacity).await
    }
}

async fn paged_group(path: &Path, gate: Arc<PagedReadGate>) -> Result<Arc<SharedStorageGroup>> {
    let original = uncertain_indexes(path, Arc::new(AtomicBool::new(false)), None).await?;
    let work = Arc::new(drasi_core::computation::ComputationIoScope::default());
    let cleanup = Arc::new(PagedCleanup {
        inner: original.cleanup().expect("cleanup").clone(),
        work: work.clone(),
    });
    let outbox = Arc::new(PagedOutbox {
        inner: original.outbox_writer().expect("outbox").clone(),
        gate,
        work,
    });
    let control = original.indexes().session_control.clone();
    SharedStorageGroup::new(
        "query-graph",
        ComponentId::try_new("query")?,
        wrapped_indexes(original, control, outbox)?.with_cleanup(cleanup),
    )
}

async fn paged_channel(group: &SharedStorageGroup) -> Result<Arc<QosChannel>> {
    Ok(group
        .channel_with_page_limits(
            definition(true, RetentionPolicy::Backpressure, 2),
            "paged",
            codec()?,
            options(),
            drasi_core::interface::OutboxPageLimits {
                max_records: NonZeroUsize::new(1).expect("page count"),
                max_bytes: NonZeroUsize::new(1).expect("page bytes"),
            },
        )
        .await?)
}

async fn paged_ack(
    receiver: &mut dyn EnvelopeReceiver,
    sequence: u64,
) -> Result<Box<dyn Acknowledgement>> {
    let delivery = tokio::time::timeout(Duration::from_secs(5), receiver.receive())
        .await??
        .expect("shared output");
    assert_eq!(delivery.envelope().system().sequence(), sequence);
    Ok(delivery.into_parts().1.expect("handling acknowledgement"))
}

#[tokio::test]
async fn paged_shared_startup_holds_the_group_gate_across_every_page() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    let gate = Arc::new(PagedReadGate::default());
    let group = paged_group(directory.path(), gate.clone()).await?;
    let channel = paged_channel(&group).await?;
    for input in &inputs {
        channel.publish(input).await?;
    }
    channel.shutdown().await?;
    gate.after.store(1, Ordering::SeqCst);
    let processor = drasi_core::computation::ComputationTransaction::try_new(
        group.create_indexes("query-graph", "query").await?,
    )?;
    let (restored, writer) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            paged_channel(&group),
            async {
                gate.entered.notified().await;
                let write = processor.run(async { Ok(()) });
                tokio::pin!(write);
                tokio::select! {
                    result = &mut write => anyhow::bail!("writer bypassed a later reconstruction page: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
                gate.resume.notify_one();
                write.await?;
                Ok::<_, anyhow::Error>(())
            }
        )
    }).await?;
    writer?;
    let restored = restored?;
    assert_eq!(restored.progress().await?.accepted, 2);
    restored.shutdown().await?;
    processor.shutdown().await?;
    group.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn paged_shared_refill_waits_without_holding_channel_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    let group = paged_group(directory.path(), Arc::new(PagedReadGate::default())).await?;
    let channel = paged_channel(&group).await?;
    for input in &inputs {
        channel.publish(input).await?;
    }
    let mut pipe = shared_endpoint(&group, &channel, "fast")?;
    let mut receiver = pipe.pipe.take_receiver()?;
    let processor = drasi_core::computation::ComputationTransaction::try_new(
        group.create_indexes("query-graph", "query").await?,
    )?;
    let entered = tokio::sync::Notify::new();
    let waiting = tokio::sync::Notify::new();
    let (writer, delivery) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            processor.run(async {
                entered.notify_one();
                waiting.notified().await;
                assert_eq!(
                    channel
                        .progress()
                        .await
                        .map_err(IndexError::other)?
                        .accepted,
                    2
                );
                Ok(())
            }),
            async {
                entered.notified().await;
                let receive = receiver.receive();
                tokio::pin!(receive);
                assert!(futures::poll!(&mut receive).is_pending());
                waiting.notify_one();
                receive.await
            }
        )
    })
    .await?;
    writer?;
    let delivery = delivery?.expect("first output");
    assert_eq!(delivery.envelope().system().sequence(), 1);
    delivery
        .into_parts()
        .1
        .expect("ack")
        .complete(HandlingOutcome::Handled)
        .await?;
    paged_ack(receiver.as_mut(), 2)
        .await?
        .complete(HandlingOutcome::Handled)
        .await?;
    channel.shutdown().await?;
    processor.shutdown().await?;
    group.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn paged_shared_refill_failure_and_cancellation_preserve_unhandled_history() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    for cancel in [false, true] {
        let path = directory
            .path()
            .join(if cancel { "cancel" } else { "failure" });
        let gate = Arc::new(PagedReadGate::default());
        let group = paged_group(&path, gate.clone()).await?;
        let channel = paged_channel(&group).await?;
        for input in &inputs {
            channel.publish(input).await?;
        }
        let mut pipe = shared_endpoint(&group, &channel, "fast")?;
        let mut receiver = pipe.pipe.take_receiver()?;
        paged_ack(receiver.as_mut(), 1)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        gate.after.store(1, Ordering::SeqCst);
        gate.fail.store(!cancel, Ordering::SeqCst);
        let mut receive = Box::pin(receiver.receive());
        tokio::select! {
            result = &mut receive => anyhow::bail!("refill did not reach its owned page read: {}", result.is_ok()),
            _ = gate.entered.notified() => {}
        }
        if cancel {
            drop(receive);
            assert!(
                channel.progress().await.is_err(),
                "interrupted shared transactions must fence delivery"
            );
            let closing = channel.shutdown();
            tokio::pin!(closing);
            tokio::select! {
                result = &mut closing => anyhow::bail!("cleanup abandoned actual page I/O: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            gate.resume.notify_one();
            tokio::time::timeout(Duration::from_secs(5), closing).await??;
        } else {
            gate.resume.notify_one();
            assert!(receive.await.is_err());
            assert_eq!(channel.progress().await?.processed["fast"], 1);
            paged_ack(receiver.as_mut(), 2)
                .await?
                .complete(HandlingOutcome::Handled)
                .await?;
            channel.shutdown().await?;
        }
        group.shutdown().await?;
        drop(receiver);
        drop(pipe);
        drop(channel);
        drop(group);
        let group = paged_group(&path, Arc::new(PagedReadGate::default())).await?;
        let channel = paged_channel(&group).await?;
        assert_eq!(channel.progress().await?.accepted, 2);
        assert_eq!(
            channel.progress().await?.processed["fast"],
            if cancel { 1 } else { 2 }
        );
        if cancel {
            let mut pipe = shared_endpoint(&group, &channel, "fast")?;
            let mut receiver = pipe.pipe.take_receiver()?;
            paged_ack(receiver.as_mut(), 2)
                .await?
                .complete(HandlingOutcome::Handled)
                .await?;
        }
        channel.shutdown().await?;
        group.shutdown().await?;
    }
    Ok(())
}

struct FailSecondJournalAppend {
    inner: Arc<dyn OutboxWriter>,
    staged: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl OutboxWriter for FailSecondJournalAppend {
    async fn append(&self, key: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        self.inner.append(key, sequence, data).await
    }
    async fn read_from(&self, key: &str, after: u64) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_from(key, after).await
    }
    async fn read_latest_sequence(&self, key: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(key).await
    }
    async fn clear(&self, key: &str) -> Result<(), IndexError> {
        self.inner.clear(key).await
    }
    async fn append_and_trim(
        &self,
        key: &str,
        sequence: u64,
        data: &[u8],
        retain_from: u64,
    ) -> Result<usize, IndexError> {
        let removed = self
            .inner
            .append_and_trim(key, sequence, data, retain_from)
            .await?;
        // Select namespaced journal events, not producer output or journal metadata.
        if key.starts_with("drasi_group_v1_journal_") && key.ends_with("_6576656e7473") {
            let mut staged = self.staged.lock().expect("staged journal writes");
            staged.push(key.to_owned());
            if staged.len() == 2 {
                return Err(IndexError::IOError);
            }
        }
        Ok(removed)
    }
    async fn trim_before(&self, key: &str, retain_from: u64) -> Result<usize, IndexError> {
        self.inner.trim_before(key, retain_from).await
    }
    async fn trim_to_capacity(&self, key: &str, capacity: usize) -> Result<usize, IndexError> {
        self.inner.trim_to_capacity(key, capacity).await
    }
}

#[derive(Default)]
struct Hold {
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

struct SharedSource {
    inner: BindingSource,
    release: Option<Arc<Hold>>,
}

#[async_trait]
impl ComputationComponent for SharedSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for SharedSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        if let Some(hold) = self.release.take() {
            hold.resume.notified().await;
        }
        self.inner.next().await
    }
}

struct SharedSink {
    inner: BindingSink,
    hold: Option<Arc<Hold>>,
}

#[async_trait]
impl ComputationComponent for SharedSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for SharedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        if let Some(hold) = self.hold.take() {
            hold.entered.notify_one();
            hold.resume.notified().await;
        }
        if input.envelope.changes().schema() == GraphChangeCodec::schema().descriptor() {
            assert_eq!(GraphChangeCodec::decode_changes(&input.envelope)?.len(), 1);
            self.inner.seen.rows.lock().expect("observations").push((
                self.inner.descriptor.id().to_string(),
                self.inner.destination_url.clone(),
                input.envelope.system().sequence(),
            ));
            self.inner.seen.changed.notify_one();
            Ok(())
        } else {
            self.inner.handle(input).await
        }
    }
}

fn shared_definition() -> Result<QosChannelDefinition> {
    let mut definition = definition(true, RetentionPolicy::Backpressure, 1);
    definition.stream = StreamId::try_new("query/out")?;
    definition.subscribers = BTreeMap::from([("target".into(), SubscriptionStart::Earliest)]);
    Ok(definition)
}

fn shared_endpoint(
    group: &Arc<SharedStorageGroup>,
    channel: &Arc<QosChannel>,
    subscriber: &str,
) -> Result<ProvidedPipe> {
    let journal = ResourceId::try_new("channel")?;
    let storage = ResourceId::try_new("storage")?;
    Ok(channel
        .definition()
        .pipe(journal.clone(), subscriber)
        .with_shared_storage(storage.clone())
        .create_with_resources(&BTreeMap::from([
            (journal, channel.resource()),
            (storage, group.resource()),
        ]))?)
}

async fn shared_group(
    root: &Path,
    fail: Arc<AtomicBool>,
    gate: Option<Arc<CommitGate>>,
) -> Result<(Arc<SharedStorageGroup>, [Arc<QosChannel>; 2])> {
    shared_journals(root, shared_definition()?, fail, gate).await
}

async fn shared_journals(
    root: &Path,
    definition: QosChannelDefinition,
    fail: Arc<AtomicBool>,
    gate: Option<Arc<CommitGate>>,
) -> Result<(Arc<SharedStorageGroup>, [Arc<QosChannel>; 2])> {
    shared_journals_after(root, definition, fail, gate, 0).await
}

async fn shared_journals_after(
    root: &Path,
    definition: QosChannelDefinition,
    fail: Arc<AtomicBool>,
    gate: Option<Arc<CommitGate>>,
    skip: usize,
) -> Result<(Arc<SharedStorageGroup>, [Arc<QosChannel>; 2])> {
    let group = SharedStorageGroup::new(
        "query-graph",
        ComponentId::try_new("query")?,
        uncertain_indexes_after(root, fail, gate, skip).await?,
    )?;
    let left = group
        .channel(definition.clone(), "left", codec()?, options())
        .await?;
    let right = group
        .channel(definition, "right", codec()?, options())
        .await?;
    Ok((group, [left, right]))
}

fn shared_graph(
    group: &Arc<SharedStorageGroup>,
    channels: &[Arc<QosChannel>; 2],
    events: usize,
    scheduled: bool,
    release: Option<Arc<Hold>>,
    slow: Option<Arc<Hold>>,
    seen: Arc<BindingObservations>,
) -> Result<ComputationGraph> {
    shared_graph_for(
        group,
        channels,
        events,
        SharedProducer::Query(scheduled),
        release,
        slow,
        seen,
    )
}

#[derive(Clone, Copy, Debug)]
enum SharedProducer {
    Query(bool),
    Middleware,
    Linear,
}

fn shared_graph_for(
    group: &Arc<SharedStorageGroup>,
    channels: &[Arc<QosChannel>; 2],
    events: usize,
    producer: SharedProducer,
    release: Option<Arc<Hold>>,
    slow: Option<Arc<Hold>>,
    seen: Arc<BindingObservations>,
) -> Result<ComputationGraph> {
    let storage = ResourceId::try_new("shared-storage")?;
    let multicast = Arc::ptr_eq(&channels[0], &channels[1]);
    let pipes = ["left", "right"].map(|name| {
        let resource =
            ResourceId::try_new(if multicast { "left" } else { name }).expect("fixture resource");
        let channel = &channels[usize::from(name == "right")];
        let subscriber = if channel.definition().subscribers.contains_key(name) {
            name
        } else {
            "target"
        };
        (
            channel.clone(),
            channel
                .definition()
                .pipe(resource, subscriber)
                .with_shared_storage(storage.clone()),
        )
    });
    shared_graph_with_pipes(group, &pipes, events, producer, release, slow, seen)
}

fn shared_graph_with_pipes(
    group: &Arc<SharedStorageGroup>,
    pipes: &[(Arc<QosChannel>, QosPipeConfig); 2],
    events: usize,
    producer: SharedProducer,
    release: Option<Arc<Hold>>,
    slow: Option<Arc<Hold>>,
    seen: Arc<BindingObservations>,
) -> Result<ComputationGraph> {
    let ep = |component: &str, port: &str| -> Result<Endpoint> {
        Ok(Endpoint::new(
            ComponentId::try_new(component)?,
            PortId::try_new(port)?,
        ))
    };
    let descriptor =
        |id: &str, port: &str, direction, schema: Arc<Schema>| -> Result<ComponentDescriptor> {
            Ok(ComponentDescriptor::try_new(
                ComponentId::try_new(id)?,
                vec![PortDescriptor::new(
                    PortId::try_new(port)?,
                    direction,
                    schema.descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?)
        };
    let query = if matches!(producer, SharedProducer::Query(true)) {
        "MATCH (n:Item) WHERE drasi.trueLater(true, 2000) RETURN n"
    } else {
        "MATCH (n:Item) RETURN n"
    };
    let definition = ContinuousQueryDefinition {
        graph_id: "query-graph".into(),
        id: ComponentId::try_new("query")?,
        query: query.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out")?,
        outbox_capacity: NonZeroUsize::new(8).expect("output capacity"),
    };
    let factory = Arc::new(ContinuousQueryFactory::default());
    let storage = ResourceId::try_new("shared-storage")?;
    let specification = ComponentSpecification {
        descriptor: definition.descriptor(),
        role: ComponentRole::Query,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: BTreeMap::from([
            (
                Arc::from("query"),
                ConfigurationValue::Literal(serde_json::json!(query)),
            ),
            (
                Arc::from("stream"),
                ConfigurationValue::Literal(serde_json::json!("query/out")),
            ),
            (
                Arc::from("outbox_capacity"),
                ConfigurationValue::Literal(serde_json::json!(8)),
            ),
        ]),
        dependencies: BTreeMap::from([(Arc::from("indexes"), vec![storage.clone()])]),
    };
    let mut builder = ComputationGraph::builder("query-graph")
        .declare_resource(ResourceSpecification {
            id: storage.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Graph,
            binding: "shared-storage".into(),
        })?
        .provide_resource(storage.clone(), group.resource())?;
    let output_schema = match producer {
        SharedProducer::Query(_) => {
            builder = builder.component(specification, factory);
            QueryChangeCodec::schema()
        }
        SharedProducer::Middleware | SharedProducer::Linear => {
            let registry_id = ResourceId::try_new("steps")?;
            let middleware = Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new());
            let (specification, factory, resource): (_, Arc<dyn ComponentFactory>, _) = if matches!(
                producer,
                SharedProducer::Middleware
            ) {
                let definition = MiddlewareTransformerDefinition {
                    id: ComponentId::try_new("query")?,
                    output_stream: StreamId::try_new("query/out")?,
                    middleware: vec![],
                    pipeline: vec![],
                };
                (
                    definition.durable_specification(
                        registry_id.clone(),
                        storage.clone(),
                        DurableMiddlewareOptions {
                            graph_id: "query-graph".into(),
                            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
                        },
                    )?,
                    Arc::new(MiddlewareTransformerFactory::default()),
                    ResourceHandle::new(
                        ResourceRole::Middleware,
                        Arc::new(MiddlewareRegistryResource(middleware)),
                    ),
                )
            } else {
                let registry = Arc::new(TransactionalTransformerRegistry::standard(middleware));
                let definition = TransactionTransformerDefinition {
                        graph_id: "query-graph".into(), id: ComponentId::try_new("query")?,
                        output_stream: StreamId::try_new("query/out")?,
                        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
                        steps: ["first", "second"].into_iter().map(|name| Ok(TransactionStepDefinition {
                            id: ComponentId::try_new(name)?,
                            implementation: ImplementationIdentity::try_new("drasi/middleware-transformer", "1")?,
                            configuration_version: 1,
                            configuration: serde_json::json!({"middleware": [], "pipeline": []}),
                        })).collect::<Result<_>>()?,
                    };
                (
                    definition.specification(&registry, registry_id.clone(), storage.clone())?,
                    Arc::new(TransactionTransformerFactory::default()),
                    ResourceHandle::new(
                        ResourceRole::Component,
                        Arc::new(TransactionalTransformerRegistryResource(registry)),
                    ),
                )
            };
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: registry_id.clone(),
                    role: resource.role(),
                    ownership: ResourceOwnership::Borrowed,
                    binding: "steps".into(),
                })?
                .provide_resource(registry_id, resource)?
                .component(specification, factory);
            GraphChangeCodec::schema()
        }
    };
    builder = builder
        .source(Box::new(SharedSource {
            inner: BindingSource {
                descriptor: descriptor(
                    "source",
                    "out",
                    PortDirection::Output,
                    GraphChangeCodec::schema(),
                )?,
                events: (1..=events)
                    .map(|index| event(index as u64))
                    .collect::<Result<_>>()?,
            },
            release,
        }))
        .bind_stream(ep("source", "out")?, StreamId::try_new("source/out")?)
        .bind_stream(ep("query", "out")?, StreamId::try_new("query/out")?)
        .connect(
            EdgeDefinition::new(ep("source", "out")?, ep("query", "in")?),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
    for (index, (name, (channel, pipe))) in ["left", "right"].into_iter().zip(pipes).enumerate() {
        let multicast = Arc::ptr_eq(&pipes[0].0, &pipes[1].0);
        let resource = pipe.resource.clone();
        if !multicast || index == 0 {
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Graph,
                    binding: name.into(),
                })?
                .provide_resource(resource.clone(), channel.resource())?;
        }
        builder = builder
            .sink(Box::new(SharedSink {
                inner: BindingSink {
                    descriptor: descriptor(
                        name,
                        "in",
                        PortDirection::Input,
                        output_schema.clone(),
                    )?,
                    destination_url: name.into(),
                    seen: seen.clone(),
                },
                hold: if index == 1 { slow.clone() } else { None },
            }))
            .connect(
                EdgeDefinition::new(ep("query", "out")?, ep(name, "in")?),
                Box::new(pipe.clone()),
            );
    }
    Ok(builder.build()?)
}

#[tokio::test]
async fn shared_graph_query_factory_delivers_live_and_scheduled_output_and_reopens() -> Result<()> {
    for scheduled in [false, true] {
        let directory = tempfile::tempdir()?;
        let seen = Arc::new(BindingObservations::default());
        for events in [1, 0] {
            let (group, channels) =
                shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
            let mut graph = shared_graph(
                &group,
                &channels,
                events,
                scheduled,
                None,
                None,
                seen.clone(),
            )?;
            tokio::time::timeout(Duration::from_secs(20), graph.start()?).await??;
            assert_eq!(seen.rows.lock().unwrap().len(), 2);
            for channel in &channels {
                assert_eq!(channel.progress().await?.accepted, 1);
                assert_eq!(channel.progress().await?.processed["target"], 1);
            }
            graph.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn shared_graph_mixed_output_stores_recover_partial_publication() -> Result<()> {
    for producer in [
        SharedProducer::Query(false),
        SharedProducer::Query(true),
        SharedProducer::Middleware,
        SharedProducer::Linear,
    ] {
        for before_commit in [false, true] {
            let directory = tempfile::tempdir()?;
            let seen = Arc::new(BindingObservations::default());
            for interrupted in [true, false] {
                let group = SharedStorageGroup::new(
                    "query-graph",
                    ComponentId::try_new("query")?,
                    uncertain_indexes(
                        &directory.path().join("shared"),
                        Arc::new(AtomicBool::new(false)),
                        None,
                    )
                    .await?,
                )?;
                let left = group
                    .channel(shared_definition()?, "left", codec()?, options())
                    .await?;
                let fail = Arc::new(AtomicBool::new(false));
                let gate = Arc::new(CommitGate {
                    before_commit,
                    entered: tokio::sync::Notify::new(),
                    resume: tokio::sync::Notify::new(),
                });
                let right = uncertain_channel(
                    &directory.path().join("separate"),
                    shared_definition()?,
                    fail.clone(),
                    Some(gate.clone()),
                )
                .await?;
                right.enable_replay(options()).await?;
                if !interrupted {
                    assert_eq!(left.progress().await?.accepted, 1);
                    assert_eq!(left.progress().await?.processed["target"], 1);
                    assert_eq!(right.progress().await?.accepted, u64::from(!before_commit));
                }
                let pipes = [
                    (
                        left.clone(),
                        left.definition()
                            .pipe(ResourceId::try_new("left")?, "target")
                            .with_shared_storage(ResourceId::try_new("shared-storage")?),
                    ),
                    (
                        right.clone(),
                        right
                            .definition()
                            .pipe(ResourceId::try_new("right")?, "target"),
                    ),
                ];
                let release = Arc::new(Hold::default());
                let mut graph = shared_graph_with_pipes(
                    &group,
                    &pipes,
                    1,
                    producer,
                    interrupted.then(|| release.clone()),
                    None,
                    seen.clone(),
                )?;
                if interrupted {
                    let run = graph.start()?;
                    let control = run.control();
                    let (result, inspected) =
                        tokio::time::timeout(Duration::from_secs(20), async {
                            tokio::join!(run, async {
                                let inspected = async {
                                    assert_eq!(
                                        control.startup_report().await?.summary,
                                        OperationSummary::Completed
                                    );
                                    fail.store(true, Ordering::Release);
                                    release.resume.notify_one();
                                    gate.entered.notified().await;
                                    while left.progress().await?.processed["target"] != 1 {
                                        tokio::time::sleep(Duration::from_millis(1)).await;
                                    }
                                    assert_eq!(left.progress().await?.accepted, 1);
                                    assert_eq!(seen.rows.lock().unwrap().len(), 1);
                                    Ok::<_, anyhow::Error>(())
                                }
                                .await;
                                control.cancel();
                                inspected
                            })
                        })
                        .await?;
                    inspected?;
                    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
                } else {
                    tokio::time::timeout(Duration::from_secs(20), graph.start()?).await??;
                    assert_eq!(seen.rows.lock().unwrap().len(), 2);
                    for channel in [&left, &right] {
                        assert_eq!(channel.progress().await?.accepted, 1);
                        assert_eq!(channel.progress().await?.processed["target"], 1);
                    }
                }
                graph.shutdown().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn shared_graph_reconciliation_replaces_producers_and_preserves_owner() -> Result<()> {
    for producer in [
        SharedProducer::Query(false),
        SharedProducer::Query(true),
        SharedProducer::Middleware,
        SharedProducer::Linear,
    ] {
        let directory = tempfile::tempdir()?;
        let (group, channels) =
            shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
        let seen = Arc::new(BindingObservations::default());
        let mut graph = shared_graph_for(&group, &channels, 1, producer, None, None, seen.clone())?;
        let run = graph.run()?;
        let control = run.control();
        let (result, inspected) = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(run, async {
                let inspected = async {
                    control.deployment_report().await?;
                    let revision = control.desired_snapshot().revision;
                    assert_eq!(
                        control
                            .start_components(revision, GraphSelection::All)
                            .await?
                            .summary,
                        OperationSummary::Completed
                    );
                    let mut observed = control.subscribe_observed();
                    observed
                        .wait_for(|state| state.components.values().all(|node| node.exhausted))
                        .await?;
                    assert_eq!(seen.rows.lock().unwrap().len(), 2);
                    let storage = ResourceId::try_new("shared-storage")?;
                    assert!(control
                        .preview(
                            revision,
                            vec![DesiredMutation::RemoveResource {
                                resource: storage.clone(),
                                policy: RemovalPolicy::Reject,
                            }]
                        )
                        .await
                        .is_err());
                    assert_eq!(control.desired_snapshot().revision, revision);
                    let id = ComponentId::try_new("query")?;
                    let desired = control
                        .desired_snapshot()
                        .select(GraphSelection::Exact(vec![id.clone()]))?
                        .components
                        .remove(0);
                    let generation = observed.borrow().components[&id].generation;
                    let preview = control
                        .preview(revision, vec![DesiredMutation::ReplaceComponent(desired)])
                        .await?;
                    let report = control
                        .reconcile(preview, TopologyBindings::default())
                        .await?;
                    assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                    assert!(report.committed);
                    assert_ne!(control.observed().components[&id].generation, generation);
                    for channel in &channels {
                        assert_eq!(channel.progress().await?.accepted, 1);
                        assert_eq!(channel.progress().await?.processed["target"], 1);
                    }
                    let preview = control
                        .preview(
                            report.revision,
                            vec![DesiredMutation::RemoveComponents {
                                selection: GraphSelection::All,
                                policy: RemovalPolicy::Drain,
                            }],
                        )
                        .await?;
                    let report = control
                        .reconcile(preview, TopologyBindings::default())
                        .await?;
                    assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                    assert_eq!(report.removed.len(), 4);
                    drop(group.create_indexes("query-graph", "query").await?);
                    let independent = group
                        .channel(shared_definition()?, "after-removal", codec()?, options())
                        .await?;
                    independent.shutdown().await?;
                    let preview = control
                        .preview(
                            report.revision,
                            vec![DesiredMutation::RemoveResource {
                                resource: storage,
                                policy: RemovalPolicy::Reject,
                            }],
                        )
                        .await?;
                    let report = control
                        .reconcile(preview, TopologyBindings::default())
                        .await?;
                    assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                    assert!(group
                        .channel(
                            shared_definition()?,
                            "after-owner-removal",
                            codec()?,
                            options(),
                        )
                        .await
                        .is_err());
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                control.cancel();
                inspected
            })
        })
        .await?;
        inspected?;
        assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
        graph.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn shared_graph_capacity_wait_does_not_hold_the_storage_transaction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (group, channels) =
        shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
    let seen = Arc::new(BindingObservations::default());
    let slow = Arc::new(Hold::default());
    let mut graph = shared_graph(
        &group,
        &channels,
        2,
        false,
        None,
        Some(slow.clone()),
        seen.clone(),
    )?;
    let run = graph.start()?;
    let (result, observed) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(run, async {
            slow.entered.notified().await;
            assert_eq!(channels[1].progress().await?.accepted, 1);
            let independent = tokio::time::timeout(
                Duration::from_secs(3),
                group.channel(shared_definition()?, "independent", codec()?, options()),
            )
            .await??;
            independent.shutdown().await?;
            slow.resume.notify_one();
            Ok::<_, anyhow::Error>(())
        })
    })
    .await?;
    observed?;
    result?;
    assert_eq!(seen.rows.lock().unwrap().len(), 4);
    for channel in &channels {
        assert_eq!(channel.progress().await?.accepted, 2);
    }
    graph.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_graph_cancelled_commit_reconstructs_all_output_branches() -> Result<()> {
    for producer in [
        SharedProducer::Query(false),
        SharedProducer::Query(true),
        SharedProducer::Middleware,
        SharedProducer::Linear,
    ] {
        for before_commit in [false, true] {
            let directory = tempfile::tempdir()?;
            let fail = Arc::new(AtomicBool::new(false));
            let gate = Arc::new(CommitGate {
                before_commit,
                entered: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            });
            let (group, channels) = shared_journals_after(
                directory.path(),
                shared_definition()?,
                fail.clone(),
                Some(gate.clone()),
                usize::from(matches!(producer, SharedProducer::Query(true))),
            )
            .await?;
            let release = Arc::new(Hold::default());
            let seen = Arc::new(BindingObservations::default());
            let mut graph = shared_graph_for(
                &group,
                &channels,
                1,
                producer,
                Some(release.clone()),
                None,
                seen.clone(),
            )?;
            let run = graph.start()?;
            let control = run.control();
            let (result, observed) = tokio::time::timeout(Duration::from_secs(20), async {
                tokio::join!(run, async {
                    assert_eq!(
                        control.startup_report().await?.summary,
                        OperationSummary::Completed
                    );
                    fail.store(true, Ordering::Release);
                    release.resume.notify_one();
                    gate.entered.notified().await;
                    for channel in &channels {
                        assert_eq!(channel.progress().await?.accepted, 0);
                    }
                    control.cancel();
                    Ok::<_, anyhow::Error>(())
                })
            })
            .await?;
            observed?;
            assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
            assert!(seen.rows.lock().unwrap().is_empty());
            graph.shutdown().await?;
            drop((graph, group, channels, control));

            let (group, channels) =
                shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
            for channel in &channels {
                assert_eq!(
                    channel.progress().await?.accepted,
                    u64::from(!before_commit)
                );
            }
            let mut graph =
                shared_graph_for(&group, &channels, 1, producer, None, None, seen.clone())?;
            tokio::time::timeout(Duration::from_secs(20), graph.start()?).await??;
            assert_eq!(seen.rows.lock().unwrap().len(), 2);
            for channel in &channels {
                assert_eq!(channel.progress().await?.accepted, 1);
                assert_eq!(channel.progress().await?.processed["target"], 1);
            }
            graph.shutdown().await?;
        }
    }
    Ok(())
}

async fn admitted_inputs(path: &Path) -> Result<Vec<ChangeEnvelope>> {
    let definition = definition(true, RetentionPolicy::Backpressure, 2);
    let source = persistent(path, definition.clone()).await?;
    source.enable_admission(admission_options()?).await?;
    let session = source
        .register_producer(ComponentId::try_new("client")?)
        .await?;
    let mut connection = endpoint(&source, &definition, "fast", false)?;
    let mut receiver = connection.pipe.take_receiver()?;
    let mut output = Vec::new();
    for sequence in 1..=2 {
        source.admit(&session, sequence, &event(sequence)?).await?;
        let delivery = receiver.receive().await?.expect("admitted input");
        let (envelope, acknowledgement) = delivery.into_parts();
        output.push(envelope);
        acknowledgement
            .expect("acknowledgement")
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    connection.control.cancel();
    source.shutdown().await?;
    Ok(output)
}

#[tokio::test]
async fn shared_group_failure_wakes_capacity_publisher_and_empty_receiver_waiters() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    let definition = definition(true, RetentionPolicy::Backpressure, 1);
    let fail = Arc::new(AtomicBool::new(false));
    let (group, channels) = shared_journals(
        &directory.path().join("output"),
        definition.clone(),
        fail.clone(),
        None,
    )
    .await?;
    channels[0].publish(&inputs[0]).await?;
    let left = shared_endpoint(&group, &channels[0], "fast")?;
    let slow = shared_endpoint(&group, &channels[0], "slow")?;
    let mut right = shared_endpoint(&group, &channels[1], "fast")?;
    let mut receiver = right.pipe.take_receiver()?;
    let sender = left.pipe.sender();
    let another = slow.pipe.sender();
    fail.store(true, Ordering::Release);
    let (capacity, publisher, reader, failed) =
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(
                sender.send(inputs[1].clone()),
                another.send(inputs[1].clone()),
                receiver.receive(),
                channels[1].publish(&inputs[0]),
            )
        })
        .await?;
    assert!(capacity.is_err());
    assert!(publisher.is_err());
    assert!(reader.is_err());
    assert!(matches!(failed, Err(PipeError::AcceptanceUnknown { .. })));
    for channel in &channels {
        channel.shutdown().await?;
    }
    group.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_endpoint_revocation_rejects_waiting_appends_without_acceptance() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    for cancel in [false, true] {
        for waiting_for_storage in [false, true] {
            let definition = definition(true, RetentionPolicy::Backpressure, 1);
            let path = directory
                .path()
                .join(format!("{cancel}-{waiting_for_storage}"));
            let (group, channels) = shared_journals(
                &path,
                definition.clone(),
                Arc::new(AtomicBool::new(false)),
                None,
            )
            .await?;
            let connection = shared_endpoint(&group, &channels[0], "fast")?;
            let sender = connection.pipe.sender();
            let blocker = drasi_core::computation::ComputationTransaction::try_new(
                group.create_indexes("query-graph", "query").await?,
            )?;
            let hold = Arc::new(Hold::default());
            if !waiting_for_storage {
                channels[0].publish(&inputs[0]).await?;
            }
            let (blocked, sent, ()) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    async {
                        if waiting_for_storage {
                            blocker
                                .run(async {
                                    hold.entered.notify_one();
                                    hold.resume.notified().await;
                                    Ok(())
                                })
                                .await?;
                        } else {
                            hold.entered.notify_one();
                        }
                        Ok::<_, anyhow::Error>(())
                    },
                    sender.send(inputs[usize::from(!waiting_for_storage)].clone()),
                    async {
                        hold.entered.notified().await;
                        if cancel {
                            connection.control.cancel();
                        } else {
                            connection.control.close();
                        }
                        hold.resume.notify_one();
                    }
                )
            })
            .await?;
            blocked?;
            assert!(!matches!(
                sent.expect_err("revoked sender").error,
                PipeError::AcceptanceUnknown { .. }
            ));
            assert_eq!(
                channels[0].progress().await?.accepted,
                u64::from(!waiting_for_storage)
            );
            assert!(!blocker.recovery_required());
            blocker.shutdown().await?;
            for channel in &channels {
                channel.shutdown().await?;
            }
            group.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn shared_storage_binding_requires_the_actual_group_and_declared_owner() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (left, left_channels) = shared_group(
        &directory.path().join("left"),
        Arc::new(AtomicBool::new(false)),
        None,
    )
    .await?;
    let (right, right_channels) = shared_group(
        &directory.path().join("right"),
        Arc::new(AtomicBool::new(false)),
        None,
    )
    .await?;
    assert!(left.create_indexes("another-graph", "query").await.is_err());
    assert!(left
        .create_indexes("query-graph", "another-query")
        .await
        .is_err());
    assert!(shared_graph(
        &left,
        &right_channels,
        1,
        false,
        None,
        None,
        Arc::new(BindingObservations::default())
    )
    .is_err());
    let journal = ResourceId::try_new("channel")?;
    let configuration = left_channels[0]
        .definition()
        .pipe(journal.clone(), "target");
    assert!(configuration
        .create_with_resources(&BTreeMap::from([(journal, left_channels[0].resource())]))
        .is_err());
    let valid = shared_endpoint(&left, &left_channels[0], "target")?;
    valid.control.cancel();
    for channel in left_channels.iter().chain(&right_channels) {
        assert_eq!(channel.progress().await?.accepted, 0);
        channel.shutdown().await?;
    }
    left.shutdown().await?;
    right.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_graph_stopped_producer_leaves_subscriber_acknowledgements_available() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let (group, channels) =
        shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
    let slow = Arc::new(Hold::default());
    let seen = Arc::new(BindingObservations::default());
    let mut graph = shared_graph(
        &group,
        &channels,
        1,
        false,
        None,
        Some(slow.clone()),
        seen.clone(),
    )?;
    let run = graph.start()?;
    let control = run.control();
    let (result, observed) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(run, async {
            let observed = async {
                slow.entered.notified().await;
                let revision = control.desired_snapshot().revision;
                control
                    .quiesce_components(
                        revision,
                        GraphSelection::Exact(vec![ComponentId::try_new("query")?]),
                    )
                    .await?;
                let report = control
                    .stop_components(
                        revision,
                        GraphSelection::Exact(vec![ComponentId::try_new("query")?]),
                    )
                    .await?;
                assert_eq!(report.summary, OperationSummary::Completed);
                let snapshot = control.desired_snapshot();
                let storage = ResourceId::try_new("shared-storage")?;
                assert!(snapshot
                    .edges
                    .iter()
                    .filter(|edge| edge.definition.from.component.as_str() == "query")
                    .all(|edge| edge.resources.get(&storage) == Some(&ResourceRole::IndexBackend)));
                assert_eq!(channels[1].progress().await?.processed["target"], 0);
                let independent = group
                    .channel(
                        shared_definition()?,
                        "after-producer-stop",
                        codec()?,
                        options(),
                    )
                    .await?;
                independent.shutdown().await?;
                slow.resume.notify_one();
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if channels[1].progress().await?.processed["target"] == 1 {
                            return Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await??;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            control.cancel();
            observed
        })
    })
    .await?;
    observed?;
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    assert_eq!(seen.rows.lock().unwrap().len(), 2);
    for channel in &channels {
        assert_eq!(channel.progress().await?.processed["target"], 1);
    }
    graph.shutdown().await?;
    Ok(())
}

const SHARED_CRASH_ROOT: &str = "DRASI_SHARED_QOS_CRASH_ROOT";
const SHARED_CRASH_BEFORE: &str = "DRASI_SHARED_QOS_CRASH_BEFORE";
const SHARED_CRASH_PRODUCER: &str = "DRASI_SHARED_QOS_CRASH_PRODUCER";

#[tokio::test]
async fn shared_acknowledgement_and_retirement_resolve_interrupted_commits() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    for retire in [false, true] {
        for cancel in [false, true] {
            for before_commit in [false, true] {
                let path = directory
                    .path()
                    .join(format!("{retire}-{cancel}-{before_commit}"));
                let definition = definition(true, RetentionPolicy::Backpressure, 1);
                let fail = Arc::new(AtomicBool::new(false));
                let gate = Arc::new(CommitGate {
                    before_commit,
                    entered: tokio::sync::Notify::new(),
                    resume: tokio::sync::Notify::new(),
                });
                let (group, channels) =
                    shared_journals(&path, definition.clone(), fail.clone(), Some(gate.clone()))
                        .await?;
                channels[0].publish(&inputs[0]).await?;
                let mut connection = shared_endpoint(&group, &channels[0], "fast")?;
                let mut receiver = connection.pipe.take_receiver()?;
                let acknowledgement = receiver
                    .receive()
                    .await?
                    .expect("input")
                    .into_parts()
                    .1
                    .expect("acknowledgement");
                fail.store(true, Ordering::Release);
                let mut operation = Box::pin(async {
                    if retire {
                        channels[0].retire("slow").await
                    } else {
                        acknowledgement.complete(HandlingOutcome::Handled).await
                    }
                });
                tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::select! {
                        _ = gate.entered.notified() => {},
                        result = &mut operation => panic!("commit did not pause: {result:?}"),
                    }
                })
                .await?;
                if cancel {
                    drop(operation);
                } else {
                    if !retire {
                        connection.control.cancel();
                    }
                    let subscriber = if retire { "slow" } else { "fast" };
                    assert!(
                        shared_endpoint(&group, &channels[0], subscriber).is_err(),
                        "binding must wait for committed progress/retirement"
                    );
                    gate.resume.notify_one();
                    let result = operation.await;
                    if retire {
                        result?;
                    } else {
                        assert!(
                            matches!(result, Err(PipeError::AcknowledgementUnknown { .. })),
                            "{result:?}"
                        );
                    }
                }
                assert_eq!(channels[0].progress().await.is_err(), cancel);
                for channel in &channels {
                    channel.shutdown().await?;
                }
                group.shutdown().await?;
                drop((receiver, connection, channels, group));
                let (group, channels) =
                    shared_journals(&path, definition, Arc::new(AtomicBool::new(false)), None)
                        .await?;
                let progress = channels[0].progress().await?;
                let committed = !cancel || !before_commit;
                assert_eq!(progress.accepted, 1);
                assert_eq!(progress.processed["fast"], u64::from(!retire && committed));
                assert_eq!(
                    progress.retired.iter().any(|id| id == "slow"),
                    retire && committed
                );
                for channel in &channels {
                    channel.shutdown().await?;
                }
                group.shutdown().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn shared_graph_multicast_reserves_and_appends_once_for_both_subscribers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let group = SharedStorageGroup::new(
        "query-graph",
        ComponentId::try_new("query")?,
        uncertain_indexes(directory.path(), Arc::new(AtomicBool::new(false)), None).await?,
    )?;
    let mut definition = shared_definition()?;
    definition.subscribers = BTreeMap::from([
        ("left".into(), SubscriptionStart::Earliest),
        ("right".into(), SubscriptionStart::Earliest),
    ]);
    let channel = group
        .channel(definition, "multicast", codec()?, options())
        .await?;
    let seen = Arc::new(BindingObservations::default());
    let mut graph = shared_graph(
        &group,
        &[channel.clone(), channel.clone()],
        2,
        false,
        None,
        None,
        seen.clone(),
    )?;
    tokio::time::timeout(Duration::from_secs(10), graph.start()?).await??;
    assert_eq!(seen.rows.lock().unwrap().len(), 4);
    let progress = channel.progress().await?;
    assert_eq!(progress.accepted, 2);
    assert_eq!(progress.processed["left"], 2);
    assert_eq!(progress.processed["right"], 2);
    graph.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_graph_output_encoding_failure_rolls_back_producer_and_all_journals() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (group, mut channels) =
        shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
    channels[1].shutdown().await?;
    let mut incomplete = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).expect("limit"));
    incomplete.register_schema(GraphChangeCodec::schema())?;
    channels[1] = group
        .channel(shared_definition()?, "right", incomplete, options())
        .await?;
    let seen = Arc::new(BindingObservations::default());
    let mut graph = shared_graph(&group, &channels, 1, false, None, None, seen.clone())?;
    let run = graph.start()?;
    let control = run.control();
    let (result, observed) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, async {
            let query = ComponentId::try_new("query")?;
            loop {
                if let Some(failure) = &control.observed().components[&query].failure {
                    assert_eq!(failure.phase, FailurePhase::Processing);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            control.cancel();
            Ok::<_, anyhow::Error>(())
        })
    })
    .await?;
    observed?;
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    assert!(seen.rows.lock().unwrap().is_empty());
    graph.shutdown().await?;
    drop((graph, group, channels, control));
    let (group, channels) =
        shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
    for channel in &channels {
        assert_eq!(channel.progress().await?.accepted, 0);
    }
    let mut graph = shared_graph(&group, &channels, 1, false, None, None, seen.clone())?;
    tokio::time::timeout(Duration::from_secs(10), graph.start()?).await??;
    assert_eq!(seen.rows.lock().unwrap().len(), 2);
    for channel in &channels {
        assert_eq!(channel.progress().await?.accepted, 1);
    }
    graph.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_graph_staged_journal_failure_rolls_back_all_participants() -> Result<()> {
    for producer in [
        SharedProducer::Query(false),
        SharedProducer::Query(true),
        SharedProducer::Middleware,
        SharedProducer::Linear,
    ] {
        let directory = tempfile::tempdir()?;
        let original =
            uncertain_indexes(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
        let outbox = Arc::new(FailSecondJournalAppend {
            inner: original.outbox_writer().expect("outbox").clone(),
            staged: std::sync::Mutex::new(Vec::new()),
        });
        let control = original.indexes().session_control.clone();
        let group = SharedStorageGroup::new(
            "query-graph",
            ComponentId::try_new("query")?,
            wrapped_indexes(original, control, outbox.clone())?,
        )?;
        let channels = [
            group
                .channel(shared_definition()?, "left", codec()?, options())
                .await?,
            group
                .channel(shared_definition()?, "right", codec()?, options())
                .await?,
        ];
        let seen = Arc::new(BindingObservations::default());
        let mut graph = shared_graph_for(&group, &channels, 1, producer, None, None, seen.clone())?;
        let run = graph.start()?;
        let control = run.control();
        let (result, inspected) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(run, async {
                let inspected = async {
                    let id = ComponentId::try_new("query")?;
                    let mut observed = control.subscribe_observed();
                    observed
                        .wait_for(|state| state.components[&id].failure.is_some())
                        .await?;
                    assert_eq!(
                        observed.borrow().components[&id]
                            .failure
                            .as_ref()
                            .unwrap()
                            .phase,
                        FailurePhase::Processing
                    );
                    {
                        let staged = outbox.staged.lock().unwrap();
                        assert_eq!(staged.len(), 2);
                        assert_ne!(staged[0], staged[1], "both real journal writes were staged");
                    }
                    for channel in &channels {
                        assert_eq!(channel.progress().await?.accepted, 0);
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                control.cancel();
                inspected
            })
        })
        .await?;
        inspected?;
        assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
        assert!(seen.rows.lock().unwrap().is_empty());
        graph.shutdown().await?;
        drop((graph, group, channels, control, outbox));
        let (group, channels) =
            shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
        for channel in &channels {
            assert_eq!(channel.progress().await?.accepted, 0);
        }
        let mut graph = shared_graph_for(&group, &channels, 1, producer, None, None, seen.clone())?;
        tokio::time::timeout(Duration::from_secs(15), graph.start()?).await??;
        assert_eq!(seen.rows.lock().unwrap().len(), 2);
        for channel in &channels {
            assert_eq!(channel.progress().await?.accepted, 1);
            assert_eq!(channel.progress().await?.processed["target"], 1);
        }
        graph.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn shared_group_failure_wakes_queued_writes_and_reconstruction_before_cleanup() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    let root = directory.path().join("shared");
    let definition = definition(true, RetentionPolicy::Backpressure, 1);
    let (group, channels) = shared_journals(
        &root,
        definition.clone(),
        Arc::new(AtomicBool::new(false)),
        None,
    )
    .await?;
    channels[0].publish(&inputs[0]).await?;
    let mut pipe = shared_endpoint(&group, &channels[0], "fast")?;
    let mut receiver = pipe.pipe.take_receiver()?;
    let (_, acknowledgement) = receiver
        .receive()
        .await?
        .expect("retained input")
        .into_parts();
    let processor = drasi_core::computation::ComputationTransaction::try_new(
        group.create_indexes("query-graph", "query").await?,
    )?;
    let hold = Arc::new(Hold::default());
    let mut processing = Box::pin(processor.run(async {
        hold.entered.notify_one();
        hold.resume.notified().await;
        Ok(())
    }));
    tokio::select! {
        result = &mut processing => anyhow::bail!("processor did not hold storage: {result:?}"),
        _ = hold.entered.notified() => {},
    }
    let mut publish = Box::pin(channels[1].publish(&inputs[0]));
    let acknowledgement = acknowledgement.expect("shared acknowledgement");
    let mut acknowledge = Box::pin(acknowledgement.complete(HandlingOutcome::Handled));
    let mut retire = Box::pin(channels[0].retire("slow"));
    let mut reconstruct =
        Box::pin(group.channel(definition.clone(), "waiting", codec()?, options()));
    assert!(futures::poll!(&mut publish).is_pending());
    assert!(futures::poll!(&mut acknowledge).is_pending());
    assert!(futures::poll!(&mut retire).is_pending());
    assert!(futures::poll!(&mut reconstruct).is_pending());
    group.transaction_group().expect("shared owner").cancel();
    let waiting = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(publish, acknowledge, retire, reconstruct)
    })
    .await;
    hold.resume.notify_one();
    assert!(processing.await.is_err());
    processor.shutdown().await?;
    pipe.control.cancel();
    for channel in &channels {
        channel.shutdown().await?;
    }
    group.shutdown().await?;
    let (publish, acknowledge, retire, reconstruct) = waiting?;
    assert!(publish.is_err());
    assert!(acknowledge.is_err());
    assert!(retire.is_err());
    assert!(reconstruct.is_err());
    drop((reconstruct, processor, receiver, pipe, group, channels));
    let (group, channels) =
        shared_journals(&root, definition, Arc::new(AtomicBool::new(false)), None).await?;
    let progress = channels[0].progress().await?;
    assert_eq!(progress.accepted, 1);
    assert_eq!(progress.processed["fast"], 0);
    assert!(progress.retired.is_empty());
    assert_eq!(channels[1].progress().await?.accepted, 0);
    for channel in &channels {
        channel.shutdown().await?;
    }
    group.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_journal_reconstruction_waits_for_the_group_transaction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (group, channels) =
        shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
    let identity = channels[0].output_journal_identity();
    channels[0].shutdown().await?;
    let processor = drasi_core::computation::ComputationTransaction::try_new(
        group.create_indexes("query-graph", "query").await?,
    )?;
    let hold = Arc::new(Hold::default());
    let (held, restored) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(
            processor.run(async {
                hold.entered.notify_one();
                hold.resume.notified().await;
                Ok(())
            }),
            async {
                hold.entered.notified().await;
                let reopen = group.channel(shared_definition()?, "left", codec()?, options());
                tokio::pin!(reopen);
                tokio::select! {
                    result = &mut reopen => anyhow::bail!("reconstruction bypassed the shared transaction (success: {})", result.is_ok()),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
                hold.resume.notify_one();
                reopen.await.map_err(anyhow::Error::from)
            }
        )
    }).await?;
    held?;
    let restored = restored?;
    assert_eq!(restored.output_journal_identity(), identity);
    restored.shutdown().await?;
    processor.shutdown().await?;
    channels[1].shutdown().await?;
    group.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_journal_metadata_corruption_and_implicit_reconfiguration_fail_closed() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    for fault in [
        "missing",
        "header",
        "truncated",
        "extra-row",
        "head",
        "identity",
        "missing-replay",
        "cursor",
        "missing-events",
        "settings",
    ] {
        let path = directory.path().join(fault);
        let definition = definition(true, RetentionPolicy::Backpressure, 1);
        let (group, channels) = shared_journals(
            &path,
            definition.clone(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await?;
        channels[0].publish(&inputs[0]).await?;
        channels[0].shutdown().await?;
        let journal = group
            .transaction_group()
            .expect("shared owner")
            .journal_transaction("left")?;
        let outbox = journal
            .resources()
            .outbox_writer()
            .expect("journal")
            .clone();
        let original = outbox.read_from("metadata", 0).await?;
        let events = outbox.read_from("events", 0).await?;
        let mut bytes = original[0].1.to_vec();
        match fault {
            "header" => bytes[0] = b'X',
            "truncated" => bytes.truncate(11),
            "head" | "identity" | "missing-replay" | "cursor" => {
                let mut metadata: serde_json::Value = serde_json::from_slice(&bytes[12..])?;
                match fault {
                    "head" => metadata["head"] = serde_json::json!(2),
                    "identity" => {
                        metadata["replay"]["identity"] =
                            serde_json::json!("00000000-0000-0000-0000-000000000000")
                    }
                    "missing-replay" => {
                        metadata.as_object_mut().expect("metadata").remove("replay");
                    }
                    "cursor" => metadata["cursors"]["fast"]["position"] = serde_json::json!(2),
                    _ => unreachable!(),
                }
                bytes.truncate(12);
                bytes.extend_from_slice(&serde_json::to_vec(&metadata)?);
            }
            _ => {}
        }
        journal
            .run(async {
                if fault == "missing" {
                    outbox.clear("metadata").await?;
                } else {
                    outbox.append("metadata", 1, &bytes).await?;
                }
                if fault == "extra-row" {
                    outbox.append("metadata", 2, &bytes).await?;
                }
                if fault == "missing-events" {
                    outbox.clear("events").await?;
                }
                Ok(())
            })
            .await?;
        let damaged_metadata = outbox.read_from("metadata", 0).await?;
        let damaged_events = outbox.read_from("events", 0).await?;
        journal.shutdown().await?;
        drop((journal, outbox));
        let mut replay = options();
        if fault == "settings" {
            replay.receipt_capacity = NonZeroUsize::new(3).expect("capacity");
        }
        assert!(
            group
                .channel(definition.clone(), "left", codec()?, replay)
                .await
                .is_err(),
            "{fault}"
        );
        let journal = group
            .transaction_group()
            .expect("shared owner")
            .journal_transaction("left")?;
        let outbox = journal
            .resources()
            .outbox_writer()
            .expect("journal")
            .clone();
        assert_eq!(
            outbox.read_from("metadata", 0).await?,
            damaged_metadata,
            "{fault}"
        );
        assert_eq!(
            outbox.read_from("events", 0).await?,
            damaged_events,
            "{fault}"
        );
        journal
            .run(async {
                outbox.clear("metadata").await?;
                for (sequence, bytes) in &original {
                    outbox.append("metadata", *sequence, bytes).await?;
                }
                outbox.clear("events").await?;
                for (sequence, bytes) in &events {
                    outbox.append("events", *sequence, bytes).await?;
                }
                Ok(())
            })
            .await?;
        journal.shutdown().await?;
        drop((journal, outbox));
        let restored = group
            .channel(definition, "left", codec()?, options())
            .await?;
        assert_eq!(restored.progress().await?.accepted, 1);
        restored.shutdown().await?;
        channels[1].shutdown().await?;
        group.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "invoked by shared_graph_process_exit_recovers_atomic_producer_and_pipe_output"]
async fn shared_graph_crash_child() -> Result<()> {
    let path = std::path::PathBuf::from(std::env::var(SHARED_CRASH_ROOT)?);
    let producer = match std::env::var(SHARED_CRASH_PRODUCER)?.as_str() {
        "query" => SharedProducer::Query(false),
        "scheduled" => SharedProducer::Query(true),
        "middleware" => SharedProducer::Middleware,
        "linear" => SharedProducer::Linear,
        value => anyhow::bail!("unknown crash producer: {value}"),
    };
    let fail = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(CommitGate {
        before_commit: std::env::var(SHARED_CRASH_BEFORE)? == "true",
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let (group, channels) = shared_journals_after(
        &path,
        shared_definition()?,
        fail.clone(),
        Some(gate.clone()),
        usize::from(matches!(producer, SharedProducer::Query(true))),
    )
    .await?;
    let release = Arc::new(Hold::default());
    let mut graph = shared_graph_for(
        &group,
        &channels,
        1,
        producer,
        Some(release.clone()),
        None,
        Arc::new(BindingObservations::default()),
    )?;
    let run = graph.start()?;
    let control = run.control();
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = run => anyhow::bail!("graph ended before injected process exit: {result:?}"),
            inspected = async {
                assert_eq!(control.startup_report().await?.summary, OperationSummary::Completed);
                fail.store(true, Ordering::Release);
                release.resume.notify_one();
                gate.entered.notified().await;
                Ok::<_, anyhow::Error>(())
            } => {
                inspected?;
                std::process::exit(79);
            }
        }
    })
    .await?
}

#[tokio::test]
async fn shared_graph_process_exit_recovers_atomic_producer_and_pipe_output() -> Result<()> {
    use std::process::{Command, Stdio};
    for (name, producer) in [
        ("query", SharedProducer::Query(false)),
        ("scheduled", SharedProducer::Query(true)),
        ("middleware", SharedProducer::Middleware),
        ("linear", SharedProducer::Linear),
    ] {
        for before_commit in [false, true] {
            let directory = tempfile::tempdir()?;
            let child = Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "replay::shared::shared_graph_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env(SHARED_CRASH_ROOT, directory.path())
                .env(SHARED_CRASH_BEFORE, before_commit.to_string())
                .env(SHARED_CRASH_PRODUCER, name)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            require_process_exit(child, 79).await?;
            let (group, channels) =
                shared_group(directory.path(), Arc::new(AtomicBool::new(false)), None).await?;
            for channel in &channels {
                assert_eq!(
                    channel.progress().await?.accepted,
                    u64::from(!before_commit)
                );
                assert_eq!(channel.progress().await?.processed["target"], 0);
            }
            let seen = Arc::new(BindingObservations::default());
            let mut graph =
                shared_graph_for(&group, &channels, 1, producer, None, None, seen.clone())?;
            tokio::time::timeout(Duration::from_secs(20), graph.start()?).await??;
            assert_eq!(seen.rows.lock().unwrap().len(), 2);
            for channel in &channels {
                assert_eq!(channel.progress().await?.accepted, 1);
                assert_eq!(channel.progress().await?.processed["target"], 1);
            }
            graph.shutdown().await?;
        }
    }
    Ok(())
}

async fn require_process_exit(mut child: std::process::Child, code: i32) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() > deadline {
            child.kill()?;
            let output = child.wait_with_output()?;
            anyhow::bail!("shared storage crash child timed out: {output:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "invoked by shared_progress_survives_process_exit_at_commit"]
async fn shared_progress_crash_child() -> Result<()> {
    let path = std::path::PathBuf::from(std::env::var(SHARED_CRASH_ROOT)?);
    let retire = match std::env::var(SHARED_CRASH_PRODUCER)?.as_str() {
        "acknowledge" => false,
        "retire" => true,
        value => anyhow::bail!("unknown progress action: {value}"),
    };
    let fail = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(CommitGate {
        before_commit: std::env::var(SHARED_CRASH_BEFORE)? == "true",
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let (group, channels) = shared_journals(
        &path,
        definition(true, RetentionPolicy::Backpressure, 1),
        fail.clone(),
        Some(gate.clone()),
    )
    .await?;
    let operation = async {
        if retire {
            fail.store(true, Ordering::Release);
            channels[0].retire("fast").await?;
        } else {
            let mut pipe = shared_endpoint(&group, &channels[0], "fast")?;
            let mut receiver = pipe.pipe.take_receiver()?;
            let (_, acknowledgement) = receiver
                .receive()
                .await?
                .expect("retained event")
                .into_parts();
            fail.store(true, Ordering::Release);
            acknowledgement
                .expect("shared acknowledgement")
                .complete(HandlingOutcome::Handled)
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = operation => anyhow::bail!("progress finished before process exit: {result:?}"),
            _ = gate.entered.notified() => std::process::exit(80),
        }
    }).await?
}

#[tokio::test]
async fn shared_progress_survives_process_exit_at_commit() -> Result<()> {
    use std::process::{Command, Stdio};
    let directory = tempfile::tempdir()?;
    let inputs = admitted_inputs(&directory.path().join("inputs")).await?;
    for retire in [false, true] {
        for before_commit in [false, true] {
            let path = directory
                .path()
                .join(format!("progress-{retire}-{before_commit}"));
            let definition = definition(true, RetentionPolicy::Backpressure, 1);
            let (group, channels) = shared_journals(
                &path,
                definition.clone(),
                Arc::new(AtomicBool::new(false)),
                None,
            )
            .await?;
            assert_eq!(channels[0].publish(&inputs[0]).await?.position(), Some(1));
            for channel in &channels {
                channel.shutdown().await?;
            }
            group.shutdown().await?;
            drop((group, channels));
            let child = Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "replay::shared::shared_progress_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env(SHARED_CRASH_ROOT, &path)
                .env(SHARED_CRASH_BEFORE, before_commit.to_string())
                .env(
                    SHARED_CRASH_PRODUCER,
                    if retire { "retire" } else { "acknowledge" },
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            require_process_exit(child, 80).await?;
            let (group, channels) =
                shared_journals(&path, definition, Arc::new(AtomicBool::new(false)), None).await?;
            let progress = channels[0].progress().await?;
            assert_eq!(progress.accepted, 1);
            assert_eq!(progress.processed["slow"], 0);
            assert_eq!(
                progress.processed["fast"],
                u64::from(!retire && !before_commit)
            );
            assert_eq!(
                progress.retired.iter().any(|id| id == "fast"),
                retire && !before_commit
            );
            assert_eq!(channels[0].publish(&inputs[0]).await?.position(), Some(1));
            if retire && !before_commit {
                let mut pipe = shared_endpoint(&group, &channels[0], "fast")?;
                let mut receiver = pipe.pipe.take_receiver()?;
                let error = tokio::time::timeout(Duration::from_secs(3), receiver.receive())
                    .await?
                    .err()
                    .expect("retired subscriber cannot receive");
                assert!(error.to_string().contains("retired"), "{error}");
                pipe.control.cancel();
            } else if before_commit {
                let mut pipe = shared_endpoint(&group, &channels[0], "fast")?;
                let mut receiver = pipe.pipe.take_receiver()?;
                let (_, acknowledgement) =
                    tokio::time::timeout(Duration::from_secs(3), receiver.receive())
                        .await??
                        .expect("unconfirmed delivery")
                        .into_parts();
                acknowledgement
                    .expect("shared acknowledgement")
                    .complete(HandlingOutcome::Handled)
                    .await?;
                assert_eq!(channels[0].progress().await?.processed["fast"], 1);
                pipe.control.cancel();
            }
            for channel in &channels {
                channel.shutdown().await?;
            }
            group.shutdown().await?;
        }
    }
    Ok(())
}
