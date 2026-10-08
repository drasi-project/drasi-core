// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{ComputationIndexes, ComputationResource, TransactionDomain},
    interface::{FailureMode, IndexSet, SessionControl},
};
use std::collections::BTreeMap;

struct CommitBarrier {
    armed: AtomicBool,
    before_commit: bool,
    entered: Notify,
    resume: Notify,
}

struct HeldCommit {
    inner: Arc<dyn SessionControl>,
    barrier: Arc<CommitBarrier>,
}

#[async_trait]
impl SessionControl for HeldCommit {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        let hold = self.barrier.armed.swap(false, Ordering::AcqRel);
        if hold && self.barrier.before_commit {
            self.barrier.entered.notify_one();
            self.barrier.resume.notified().await;
        }
        self.inner.commit().await?;
        if hold && !self.barrier.before_commit {
            self.barrier.entered.notify_one();
            self.barrier.resume.notified().await;
        }
        Ok(())
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

async fn group(
    root: &Path,
    barrier: Arc<CommitBarrier>,
) -> Result<(Arc<SharedStorageGroup>, [Arc<QosChannel>; 2])> {
    let original = storage(root)
        .create_indexes("native-transactions", "transaction")
        .await?;
    let control: Arc<dyn SessionControl> = Arc::new(HeldCommit {
        inner: original.indexes().session_control.clone(),
        barrier,
    });
    let domain = TransactionDomain::new(control.clone());
    let set = original.indexes();
    let indexes = ComputationIndexes::try_new(
        IndexSet {
            element_index: set.element_index.clone(),
            archive_index: set.archive_index.clone(),
            result_index: set.result_index.clone(),
            future_queue: set.future_queue.clone(),
            session_control: control,
        },
        Some(domain.clone()),
        Some(ComputationResource::participating(
            original.checkpoint_store().context("checkpoint")?.clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original.outbox_writer().context("outbox")?.clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original
                .live_results_writer()
                .context("live results")?
                .clone(),
            &domain,
        )),
    )?
    .with_cleanup(original.cleanup().context("cleanup")?.clone())
    .with_durability(original.durability());
    let group = SharedStorageGroup::new("native-transactions", id("transaction"), indexes)?;
    let definition = QosChannelDefinition {
        stream: stream("transaction/out"),
        capacity: NonZeroUsize::new(2).expect("capacity"),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([("target".into(), SubscriptionStart::Earliest)]),
    };
    let replay = ReplayOptions {
        failure_scope: FailureMode::ProcessRestart,
        receipt_capacity: NonZeroUsize::new(2).expect("receipt capacity"),
    };
    let left = group
        .channel(definition.clone(), "left", codec()?, replay.clone())
        .await?;
    let right = group.channel(definition, "right", codec()?, replay).await?;
    Ok((group, [left, right]))
}

struct ReleasedSource {
    inner: StableSource,
    release: Option<Arc<Notify>>,
}

#[async_trait]
impl ComputationComponent for ReleasedSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    async fn start(&mut self) -> Result<()> {
        self.inner.start().await
    }
    async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl EnvelopeSource for ReleasedSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        if let Some(release) = self.release.take() {
            release.notified().await;
        }
        self.inner.next().await
    }
}

fn graph(
    plugin: &NativePlugin,
    group: &Arc<SharedStorageGroup>,
    channels: &[Arc<QosChannel>; 2],
    sequence: u64,
    release: Option<Arc<Notify>>,
    seen: Arc<Mutex<Vec<ChangeEnvelope>>>,
) -> Result<ComputationGraph> {
    let [left, right] = ["left", "right"].map(|name| -> Result<Box<dyn EnvelopeSink>> {
        let mut capture = HostCapture::new(seen.clone(), Arc::new(AtomicBool::new(false)));
        capture.descriptor = ComponentDescriptor::try_new(
            id(name),
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
        Ok(Box::new(capture))
    });
    graph_with_sinks(
        plugin,
        group.resource(),
        channels,
        insert(sequence, 3)?.envelope,
        release,
        [left?, right?],
        true,
    )
}

fn graph_with_sinks(
    plugin: &NativePlugin,
    storage: ResourceHandle,
    channels: &[Arc<QosChannel>; 2],
    input: ChangeEnvelope,
    release: Option<Arc<Notify>>,
    sinks: [Box<dyn EnvelopeSink>; 2],
    shared: bool,
) -> Result<ComputationGraph> {
    let indexes = ResourceId::try_new("shared")?;
    let participants = ResourceId::try_new("participants")?;
    let registry = transaction_registry(plugin)?;
    let definition = transaction_definition(plugin)?;
    let mut source = StableSource::new()?;
    source.input = Some(input);
    let mut builder = ComputationGraph::builder("native-transactions")
        .declare_resource(ResourceSpecification {
            id: indexes.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Graph,
            binding: "shared".into(),
        })?
        .provide_resource(indexes.clone(), storage)?
        .declare_resource(ResourceSpecification {
            id: participants.clone(),
            role: ResourceRole::Component,
            ownership: ResourceOwnership::Borrowed,
            binding: "participants".into(),
        })?
        .provide_resource(
            participants.clone(),
            ResourceHandle::new(
                ResourceRole::Component,
                Arc::new(TransactionalTransformerRegistryResource(registry.clone())),
            ),
        )?
        .source(Box::new(ReleasedSource {
            inner: source,
            release,
        }))
        .component(
            definition.specification(&registry, participants, indexes.clone())?,
            Arc::new(TransactionTransformerFactory::default()),
        )
        .connect(
            edge("stable", "transaction"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(endpoint("stable", "out"), stream("stable-host/out"))
        .bind_stream(endpoint("transaction", "out"), stream("transaction/out"));
    for ((name, channel), sink) in ["left", "right"].into_iter().zip(channels).zip(sinks) {
        let resource = ResourceId::try_new(name)?;
        let pipe = channel.definition().pipe(resource.clone(), "target");
        let pipe = if shared {
            pipe.with_shared_storage(indexes.clone())
        } else {
            pipe
        };
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Graph,
                binding: name.into(),
            })?
            .provide_resource(resource.clone(), channel.resource())?
            .sink(sink)
            .connect(edge("transaction", name), Box::new(pipe));
    }
    Ok(builder.build()?)
}

fn consumer_plugin() -> Result<Arc<NativePlugin>> {
    let path = std::env::var_os("DRASI_NATIVE_RECOVERY_PLUGIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let target = match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
                Some(path) if path.is_absolute() => path,
                Some(path) => workspace().join(path),
                None => workspace().join("target"),
            };
            target.join("debug/examples").join(format!(
                "{}native_recovery{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX,
            ))
        });
    computation::load(path)
}

async fn delivery_channels(
    root: &Path,
    shared: bool,
) -> Result<(ResourceHandle, [Arc<QosChannel>; 2])> {
    if shared {
        let (group, channels) = group(
            root,
            Arc::new(CommitBarrier {
                armed: AtomicBool::new(false),
                before_commit: false,
                entered: Notify::new(),
                resume: Notify::new(),
            }),
        )
        .await?;
        return Ok((group.resource(), channels));
    }
    let provider = storage(&root.join("producer"));
    let mut channels = Vec::new();
    for name in ["left", "right"] {
        let indexes = storage(&root.join("journals"))
            .create_indexes("native-transactions", name)
            .await?;
        let channel = QosChannel::persistent(
            QosChannelDefinition {
                stream: stream("transaction/out"),
                capacity: NonZeroUsize::new(2).expect("journal capacity"),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: BTreeMap::from([("target".into(), SubscriptionStart::Earliest)]),
            },
            indexes,
            codec()?,
            "journal",
        )
        .await?;
        channel
            .enable_replay(ReplayOptions {
                failure_scope: FailureMode::ProcessRestart,
                receipt_capacity: NonZeroUsize::new(2).expect("receipt capacity"),
            })
            .await?;
        channels.push(channel);
    }
    Ok((
        ResourceHandle::new(
            ResourceRole::IndexBackend,
            Arc::new(QueryIndexProviderResource(provider)),
        ),
        channels
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected two journals"))?,
    ))
}
fn multi_operation_input() -> Result<ChangeEnvelope> {
    let seed = insert(1, 3)?.envelope;
    let changes = (0..3)
        .map(|index| {
            let mut metadata = metadata();
            metadata.reference = ElementReference::new("source", &index.to_string());
            SourceChange::Insert {
                element: Element::Node {
                    metadata,
                    properties: ElementPropertyMap::from(json!({"value":3})),
                },
            }
        })
        .collect::<Vec<_>>();
    Ok(GraphChangeCodec::derive_changes(
        &seed,
        &changes,
        seed.system().stream().clone(),
        seed.system().sequence(),
    )?)
}
async fn consumer_state(root: &Path, name: &str) -> Result<Option<ElementValue>> {
    let transaction = drasi_core::computation::ComputationTransaction::try_new(
        storage(root)
            .create_indexes("native-transactions", name)
            .await?,
    )?;
    let prefix: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
    let reference = ElementReference::new(&format!("transaction-step/{prefix}/values"), "count");
    let element = transaction
        .run(async {
            Ok(transaction
                .resources()
                .indexes()
                .element_index
                .get_element(&reference)
                .await?)
        })
        .await?;
    let result = match element.as_deref() {
        Some(Element::Node { properties, .. }) => properties.get("value").cloned(),
        None => None,
        _ => anyhow::bail!("invalid consumer state"),
    };
    transaction.shutdown().await?;
    Ok(result)
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumers_resume_partial_fanout_with_shared_and_separate_journals() -> Result<()> {
    use drasi_computation_plugin_sdk::{ConsumerMode, Scope};
    let producer = plugin()?;
    let consumer = consumer_plugin()?;
    for shared in [false, true] {
        for mode in [ConsumerMode::External, ConsumerMode::Transactional] {
            let directory = directory()?;
            let root = directory.path();
            let factory = consumer
                .factories()
                .iter()
                .find(|factory| factory.consumer_mode() == Some(mode))
                .context("missing consumer fixture factory")?;
            for (generation, fail) in [(1, true), (2, false)] {
                let (storage, channels) =
                    delivery_channels(&root.join("processing"), shared).await?;
                let mut sinks: Vec<Box<dyn EnvelopeSink>> = Vec::new();
                for name in ["left", "right"] {
                    let configuration = json!({
                        "mode": if name == "left" && fail { "fail-second" } else { "normal" },
                        "path": root.join(format!("{name}.db")),
                    });
                    let sink = factory
                        .create_consumer(
                            id(name),
                            configuration,
                            Scope {
                                instance_id: "native-transactions".into(),
                                graph_id: "native-transactions".into(),
                                generation,
                            },
                            super::storage(&root.join("consumers")),
                            DeliveryOptions {
                                scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
                                max_streams: NonZeroUsize::new(2).unwrap(),
                                receipts_per_stream: NonZeroUsize::new(2).unwrap(),
                                retry: DeliveryRetryPolicy::default(),
                            },
                        )
                        .await?;
                    sinks.push(Box::new(sink));
                }
                let mut graph = graph_with_sinks(
                    &producer,
                    storage,
                    &channels,
                    multi_operation_input()?,
                    None,
                    sinks
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("expected two consumers"))?,
                    shared,
                )?;
                let result = tokio::time::timeout(TIMEOUT, graph.start()?).await?;
                if fail {
                    assert!(
                        result.is_err(),
                        "partial consumer handling must fail the graph"
                    );
                    assert_eq!(channels[0].progress().await?.processed["target"], 0);
                } else {
                    result?;
                    for channel in &channels {
                        let progress = channel.progress().await?;
                        assert_eq!(
                            progress.accepted, 1,
                            "producer replay must not append a duplicate"
                        );
                        assert_eq!(progress.processed["target"], 1);
                    }
                }
                graph.shutdown().await?;
            }
            for name in ["left", "right"] {
                if mode == ConsumerMode::Transactional {
                    assert_eq!(
                        consumer_state(&root.join("consumers"), name).await?,
                        Some(ElementValue::Integer(3))
                    );
                } else {
                    let counts: (usize, usize, usize) = rusqlite::Connection::open(root.join(format!("{name}.db")))?
                        .query_row("SELECT (SELECT count(*) FROM effects), count(*), count(DISTINCT id) FROM attempts",
                            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                    assert_eq!((counts.0, counts.2), (3, 3));
                    if name == "left" {
                        assert_eq!(counts.1, 3, "the confirmed prefix must not run twice");
                    } else {
                        // Peer cancellation may interrupt one effect/progress handoff.
                        assert!((3..=4).contains(&counts.1));
                    }
                }
            }
        }
    }
    Ok(())
}

fn assert_rows(seen: &Mutex<Vec<ChangeEnvelope>>, sequence: u64) -> Result<()> {
    let captured = seen
        .lock()
        .map_err(|_| anyhow::anyhow!("native shared capture lock poisoned"))?;
    assert_eq!(captured.len(), sequence as usize * 2);
    for (index, envelope) in captured.iter().enumerate() {
        let element = row(envelope)?;
        let expected_count = (index / 2 + 1) as i64;
        assert_eq!(integer(&element, "value")?, 20);
        assert_eq!(integer(&element, "first_count")?, expected_count);
        assert_eq!(integer(&element, "second_count")?, expected_count);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_participant_state_and_shared_journals_recover_one_atomic_commit() -> Result<()> {
    let plugin = plugin()?;
    for before_commit in [false, true] {
        let directory = directory()?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        for (sequence, interrupt) in [(1, true), (1, false), (2, false)] {
            let barrier = Arc::new(CommitBarrier {
                armed: AtomicBool::new(false),
                before_commit,
                entered: Notify::new(),
                resume: Notify::new(),
            });
            let (group, channels) = group(directory.path(), barrier.clone()).await?;
            if !interrupt {
                let expected = if sequence == 1 {
                    u64::from(!before_commit)
                } else {
                    1
                };
                for channel in &channels {
                    assert_eq!(channel.progress().await?.accepted, expected);
                }
            }
            let release = Arc::new(Notify::new());
            let mut graph = graph(
                &plugin,
                &group,
                &channels,
                sequence,
                interrupt.then(|| release.clone()),
                seen.clone(),
            )?;
            if interrupt {
                let run = graph.start()?;
                let control = run.control();
                let (result, inspected) = tokio::time::timeout(TIMEOUT, async {
                    tokio::join!(run, async {
                        let inspected = async {
                            assert_eq!(
                                control.startup_report().await?.summary,
                                OperationSummary::Completed,
                            );
                            barrier.armed.store(true, Ordering::Release);
                            release.notify_one();
                            barrier.entered.notified().await;
                            assert!(seen.lock().unwrap().is_empty());
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
                tokio::time::timeout(TIMEOUT, graph.start()?).await??;
                assert_rows(&seen, sequence)?;
                for channel in &channels {
                    assert_eq!(channel.progress().await?.accepted, sequence);
                    assert_eq!(channel.progress().await?.processed["target"], sequence);
                }
            }
            graph.shutdown().await?;
        }
    }
    Ok(())
}

const CRASH_ROOT: &str = "DRASI_NATIVE_SHARED_CRASH_ROOT";
const CRASH_BEFORE: &str = "DRASI_NATIVE_SHARED_CRASH_BEFORE";

#[tokio::test(flavor = "current_thread")]
#[ignore = "invoked by native_shared_transaction_survives_process_exit_at_commit"]
async fn native_shared_crash_child() -> Result<()> {
    let plugin = plugin()?;
    let root = PathBuf::from(std::env::var(CRASH_ROOT)?);
    let barrier = Arc::new(CommitBarrier {
        armed: AtomicBool::new(false),
        before_commit: std::env::var(CRASH_BEFORE)? == "true",
        entered: Notify::new(),
        resume: Notify::new(),
    });
    let (group, channels) = group(&root, barrier.clone()).await?;
    let release = Arc::new(Notify::new());
    let mut graph = graph(
        &plugin,
        &group,
        &channels,
        1,
        Some(release.clone()),
        Arc::new(Mutex::new(Vec::new())),
    )?;
    let run = graph.start()?;
    let control = run.control();
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            result = run => anyhow::bail!("native graph ended before crash boundary: {result:?}"),
            inspected = async {
                assert_eq!(control.startup_report().await?.summary, OperationSummary::Completed);
                barrier.armed.store(true, Ordering::Release);
                release.notify_one();
                barrier.entered.notified().await;
                Ok::<_, anyhow::Error>(())
            } => {
                inspected?;
                std::process::exit(81);
            }
        }
    })
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn native_shared_transaction_survives_process_exit_at_commit() -> Result<()> {
    use std::process::{Command, Stdio};
    let plugin = plugin()?;
    for before_commit in [false, true] {
        let directory = directory()?;
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "shared::native_shared_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CRASH_ROOT, directory.path())
            .env(CRASH_BEFORE, before_commit.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = std::time::Instant::now() + TIMEOUT + Duration::from_secs(10);
        while child.try_wait()?.is_none() {
            if std::time::Instant::now() > deadline {
                child.kill()?;
                let output = child.wait_with_output()?;
                anyhow::bail!("native shared crash child timed out: {output:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let output = child.wait_with_output()?;
        assert_eq!(output.status.code(), Some(81), "{output:?}");
        let barrier = Arc::new(CommitBarrier {
            armed: AtomicBool::new(false),
            before_commit,
            entered: Notify::new(),
            resume: Notify::new(),
        });
        let (group, channels) = group(directory.path(), barrier).await?;
        for channel in &channels {
            assert_eq!(
                channel.progress().await?.accepted,
                u64::from(!before_commit)
            );
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut graph = graph(&plugin, &group, &channels, 1, None, seen.clone())?;
        tokio::time::timeout(TIMEOUT, graph.start()?).await??;
        assert_rows(&seen, 1)?;
        for channel in &channels {
            assert_eq!(channel.progress().await?.accepted, 1);
            assert_eq!(channel.progress().await?.processed["target"], 1);
        }
        graph.shutdown().await?;
    }
    Ok(())
}
