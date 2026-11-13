// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, ComputationTransaction,
        TransactionDomain,
    },
    interface::{CheckpointStore, FailureMode, IndexError, IndexSet, SessionControl},
};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, Ordering},
};

#[path = "durable_fanout_crash.rs"]
mod crashes;
#[path = "durable_shared.rs"]
mod shared;

#[derive(Default)]
struct CommitGate {
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
    after: bool,
    require_output: bool,
}

struct HeldCommit {
    inner: Arc<dyn SessionControl>,
    gate: Arc<CommitGate>,
    checkpoints: Arc<dyn CheckpointStore>,
}

#[async_trait]
impl SessionControl for HeldCommit {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        let output = !self.gate.require_output
            || self
                .checkpoints
                .read_checkpoint(&metadata_checkpoint("\0native-query:live-row-count:v1"))
                .await?
                .is_some_and(|checkpoint| checkpoint.sequence > 0);
        let hold = output && self.gate.armed.swap(false, Ordering::AcqRel);
        if hold && !self.gate.after {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        self.inner.commit().await?;
        if hold && self.gate.after {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        Ok(())
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct Fixture {
    graph: ComputationGraph,
    channels: [Arc<QosChannel>; 2],
    seen: [Arc<Mutex<Vec<ChangeEnvelope>>>; 2],
    changed: [Arc<Notify>; 2],
}

fn codec() -> Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).expect("positive codec bound"));
    codec.register_schema(GraphChangeCodec::schema())?;
    Ok(codec)
}

fn held_indexes(
    original: ComputationIndexes,
    gate: Option<Arc<CommitGate>>,
) -> Result<ComputationIndexes> {
    Ok(if let Some(gate) = gate {
        let control: Arc<dyn SessionControl> = Arc::new(HeldCommit {
            inner: original.indexes().session_control.clone(),
            checkpoints: original
                .checkpoint_store()
                .expect("actual checkpoint store")
                .clone(),
            gate,
        });
        let domain = TransactionDomain::new(control.clone());
        let set = original.indexes();
        ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original
                    .checkpoint_store()
                    .expect("actual checkpoint store")
                    .clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("actual outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original
                    .live_results_writer()
                    .expect("actual live results")
                    .clone(),
                &domain,
            )),
        )?
        .with_cleanup(original.cleanup().expect("actual cleanup owner").clone())
        .with_durability(original.durability())
    } else {
        original
    })
}

async fn channel(
    root: &Path,
    name: &drasi_core_types::ComponentId,
    gate: Option<Arc<CommitGate>>,
) -> Result<Arc<QosChannel>> {
    let indexes = held_indexes(
        provider(&root.join("journals"))?
            .create_indexes(GRAPH, name)
            .await?,
        gate,
    )?;
    Ok(QosChannel::persistent_with_recovery(
        QosChannelDefinition {
            stream: StreamId::try_new(format!("{COMPONENT}/out"))?,
            capacity: NonZeroUsize::new(4).expect("positive channel capacity"),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("target".into(), SubscriptionStart::Earliest)]),
        },
        indexes,
        codec()?,
        "output",
        QosRecoveryOptions::Replay(ReplayOptions {
            failure_scope: FailureMode::ProcessRestart,
            receipt_capacity: NonZeroUsize::new(4).expect("positive receipt capacity"),
        }),
    )
    .await?)
}

impl Fixture {
    async fn new(root: &Path, input: Option<ChangeEnvelope>) -> Result<Self> {
        Self::with_gate(root, input, None).await
    }

    async fn with_gate(
        root: &Path,
        input: Option<ChangeEnvelope>,
        gate: Option<Arc<CommitGate>>,
    ) -> Result<Self> {
        let channels = [
            channel(
                root,
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002e7")
                    .expect("fixture UUID"),
                None,
            )
            .await?,
            channel(
                root,
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d1")
                    .expect("fixture UUID"),
                gate,
            )
            .await?,
        ];
        let changed = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        let seen = [
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        ];
        let factory = factory()?;
        let indexes = ResourceId::try_new("indexes")?;
        let mut specification = factory.specification(
            component_id()?,
            json!({"query":QUERY, "stream":format!("{COMPONENT}/out")}),
        )?;
        specification
            .dependencies
            .insert("processing".into(), vec![indexes.clone()]);
        let mut graph = ComputationGraph::builder(GRAPH)
            .declare_resource(ResourceSpecification {
                id: indexes.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "processing".into(),
            })?
            .provide_resource(
                indexes,
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(computation::NativeProcessingResource::new(provider(
                        &root.join("query"),
                    )?)),
                ),
            )?
            .source(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-source-1")
                    .expect("fixture component name"),
                Box::new(Source {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("5c0e81d4-0199-5f29-8c30-6e624342a539")?,
                        vec![port("out", PortDirection::Output)?],
                    )?,
                    envelope: input,
                }),
            )
            .component(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-component-1")
                    .expect("fixture component name"),
                specification,
                factory,
            )
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(
                        ComponentId::try_new("5c0e81d4-0199-5f29-8c30-6e624342a539")?,
                        PortId::try_new("out")?,
                    ),
                    Endpoint::new(component_id()?, PortId::try_new("in")?),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .bind_stream(
                Endpoint::new(
                    ComponentId::try_new("5c0e81d4-0199-5f29-8c30-6e624342a539")?,
                    PortId::try_new("out")?,
                ),
                StreamId::try_new("source/out")?,
            )
            .bind_stream(
                Endpoint::new(component_id()?, PortId::try_new("out")?),
                StreamId::try_new(format!("{COMPONENT}/out"))?,
            );
        for (index, name) in ["left", "right"].iter().enumerate() {
            let resource = ResourceId::try_new(*name)?;
            let owner = ComponentId::try_new(
                [
                    "00000000-0000-0000-0000-0000000002e7",
                    "00000000-0000-0000-0000-0000000001d1",
                ][index],
            )?;
            graph = graph
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Graph,
                    binding: (*name).into(),
                })?
                .provide_resource(resource.clone(), channels[index].resource())?
                .sink(
                    ComponentName::try_new(*name)?,
                    Box::new(Sink {
                        descriptor: ComponentDescriptor::try_new(
                            owner.clone(),
                            vec![port("in", PortDirection::Input)?],
                        )?,
                        received: seen[index].clone(),
                        notification: changed[index].clone(),
                    }),
                )
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(component_id()?, PortId::try_new("out")?),
                        Endpoint::new(owner, PortId::try_new("in")?),
                    ),
                    Box::new(channels[index].definition().pipe(resource, "target")),
                );
        }
        Ok(Self {
            graph: graph.build()?,
            channels,
            seen,
            changed,
        })
    }

    async fn run(&mut self, sequence: u64) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), self.graph.start()?).await??;
        for (channel, seen) in self.channels.iter().zip(&self.seen) {
            assert_eq!(channel.progress().await?.accepted, sequence);
            assert_eq!(channel.progress().await?.processed["target"], sequence);
            assert_eq!(seen.lock().expect("result capture poisoned").len(), 1);
        }
        Ok(())
    }
}

async fn actual_destinations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let mut first = Fixture::new(
        root,
        Some(
            input(
                SourceChange::Insert {
                    element: item(10, 1),
                },
                1,
            )?
            .envelope,
        ),
    )
    .await?;
    let run = first.run(1).await;
    let stopped = first.graph.shutdown().await;
    run?;
    stopped?;
    drop(first);
    let owner = ComputationTransaction::try_new(
        provider(&root.join("query"))?
            .create_indexes(GRAPH, &component_id()?)
            .await?,
    )?;
    let store = owner
        .resources()
        .checkpoint_store()
        .expect("actual checkpoint store");
    let bindings = OutputBindings::load(
        store
            .read_checkpoint(&metadata_checkpoint(OUTPUT_BINDINGS_CHECKPOINT))
            .await?
            .as_ref(),
    )?;
    assert_eq!(bindings.destinations().len(), 2);
    assert_eq!(
        bindings.destinations()[0].consumer,
        ComponentId::try_new("00000000-0000-0000-0000-0000000001d1")?
    );
    assert_eq!(
        bindings.destinations()[1].consumer,
        ComponentId::try_new("00000000-0000-0000-0000-0000000002e7")?
    );
    assert_ne!(
        bindings.destinations()[0].journal,
        bindings.destinations()[1].journal
    );
    owner.shutdown().await?;
    drop(owner);
    let mut second = Fixture::new(
        root,
        Some(
            input(
                SourceChange::Replace {
                    element: item(12, 2),
                },
                2,
            )?
            .envelope,
        ),
    )
    .await?;
    let run = second.run(2).await;
    let stopped = second.graph.shutdown().await;
    run?;
    stopped?;
    drop(second);
    let mut blocked = create(&root.join("query"), QUERY, 3)?;
    blocked.initialize().await?;
    blocked.start().await?;
    blocked.stop().await?;
    for _ in 0..2 {
        let failure = blocked
            .delete_owned_resources()
            .await
            .expect_err("bound destinations must protect their state");
        assert!(format!("{failure:#}").contains("destinations"));
    }
    let mut query = create(&root.join("query"), QUERY, 3)?;
    query.initialize().await?;
    query.start().await?;
    assert!(
        query.continue_transform().await.is_err(),
        "retained destinations were not actually rebound"
    );
    query
        .bind_output_destinations(&OutputBindings::default())
        .await?;
    assert!(query.continue_transform().await?.is_empty());
    query.stop().await?;
    query.delete_owned_resources().await?;
    drop(blocked);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_output_binds_actual_persistent_fanout_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), actual_destinations()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_output_binds_actual_persistent_fanout_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), actual_destinations()).await?
}
