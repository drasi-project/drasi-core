// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

const SHARED_ROOT: &str = "DRASI_QUERY_PLUGIN_SHARED_ROOT";
const SHARED_PHASE: &str = "DRASI_QUERY_PLUGIN_SHARED_PHASE";

struct ReleasedSource {
    source: Source,
    release: Option<Arc<Notify>>,
    finish: Option<Arc<Notify>>,
    extra: std::collections::VecDeque<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for ReleasedSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.source.descriptor()
    }
    async fn start(&mut self) -> Result<()> {
        self.source.start().await
    }
    async fn stop(&mut self) -> Result<()> {
        self.source.stop().await
    }
}

#[async_trait]
impl EnvelopeSource for ReleasedSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        if let Some(release) = self.release.take() {
            release.notified().await;
        }
        if self.source.envelope.is_none() {
            if let Some(envelope) = self.extra.pop_front() {
                return Ok(Some(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope,
                }));
            }
            if let Some(finish) = self.finish.take() {
                finish.notified().await;
            }
        }
        self.source.next().await
    }
}

struct FlowGate {
    entered: [Arc<Notify>; 2],
    release: [Arc<Notify>; 2],
}

struct HeldSink {
    sink: Sink,
    entered: Arc<Notify>,
    release: Option<Arc<Notify>>,
}

#[async_trait]
impl ComputationComponent for HeldSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.sink.descriptor()
    }
    async fn start(&mut self) -> Result<()> {
        self.sink.start().await
    }
    async fn stop(&mut self) -> Result<()> {
        self.sink.stop().await
    }
}

#[async_trait]
impl EnvelopeSink for HeldSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        if let Some(release) = self.release.take() {
            self.entered.notify_one();
            release.notified().await;
        }
        self.sink.handle(input).await
    }
}

struct SharedFixture {
    graph: ComputationGraph,
    _group: Arc<SharedStorageGroup>,
    channels: [Arc<QosChannel>; 2],
    seen: [Arc<Mutex<Vec<ChangeEnvelope>>>; 2],
    progress: Arc<QuerySourceProgress>,
    finish: Arc<Notify>,
}

impl SharedFixture {
    async fn new(
        root: &Path,
        gate: Option<Arc<CommitGate>>,
        release: Option<Arc<Notify>>,
        sequence: u64,
    ) -> Result<Self> {
        Self::with_flow(root, gate, release, sequence, None).await
    }

    async fn with_flow(
        root: &Path,
        gate: Option<Arc<CommitGate>>,
        release: Option<Arc<Notify>>,
        sequence: u64,
        flow: Option<Arc<FlowGate>>,
    ) -> Result<Self> {
        let indexes = held_indexes(
            provider(root)?
                .create_indexes(GRAPH, &component_id()?)
                .await?,
            gate,
        )?;
        let group = SharedStorageGroup::new(GRAPH, component_id()?, indexes)?;
        let definition = QosChannelDefinition {
            stream: StreamId::try_new(format!("{COMPONENT}/out"))?,
            capacity: NonZeroUsize::new(if flow.is_some() { 1 } else { 4 })
                .expect("positive channel capacity"),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("target".into(), SubscriptionStart::Earliest)]),
        };
        let replay = ReplayOptions {
            failure_scope: FailureMode::ProcessRestart,
            receipt_capacity: NonZeroUsize::new(4).expect("positive receipt capacity"),
        };
        let channels = [
            group
                .channel(definition.clone(), "left", codec()?, replay.clone())
                .await?,
            group.channel(definition, "right", codec()?, replay).await?,
        ];
        let query = factory()?.create_component_with_processing(
            component_id()?,
            json!({"query":QUERY, "stream":format!("{COMPONENT}/out")}),
            scope(1),
            group.clone(),
        )?;
        let progress = query
            .processing_progress_owner()
            .context("missing shared progress owner")?;
        let storage = ResourceId::try_new("shared")?;
        let finish = Arc::new(Notify::new());
        let mut graph = ComputationGraph::builder(GRAPH)
            .declare_resource(ResourceSpecification {
                id: storage.clone(),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Graph,
                binding: "shared".into(),
            })?
            .provide_resource(storage.clone(), group.resource())?
            .source(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-source-1")
                    .expect("fixture component name"),
                Box::new(ReleasedSource {
                    source: Source {
                        descriptor: ComponentDescriptor::try_new(
                            ComponentId::try_new("a3b1f1e3-d04c-56d4-be7f-cba6b8b9e186")?,
                            vec![port("out", PortDirection::Output)?],
                        )?,
                        envelope: Some(
                            input(
                                SourceChange::Insert {
                                    element: item(10, sequence),
                                },
                                sequence,
                            )?
                            .envelope,
                        ),
                    },
                    release,
                    finish: Some(finish.clone()),
                    extra: if flow.is_some() {
                        [input(
                            SourceChange::Replace {
                                element: item(11, 2),
                            },
                            2,
                        )?
                        .envelope]
                        .into()
                    } else {
                        Default::default()
                    },
                }),
            )
            .transformer(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-transformer-1")
                    .expect("fixture component name"),
                Box::new(query),
            )
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(
                        ComponentId::try_new("a3b1f1e3-d04c-56d4-be7f-cba6b8b9e186")?,
                        PortId::try_new("out")?,
                    ),
                    Endpoint::new(component_id()?, PortId::try_new("in")?),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .bind_stream(
                Endpoint::new(
                    ComponentId::try_new("a3b1f1e3-d04c-56d4-be7f-cba6b8b9e186")?,
                    PortId::try_new("out")?,
                ),
                StreamId::try_new("source/out")?,
            )
            .bind_stream(
                Endpoint::new(component_id()?, PortId::try_new("out")?),
                StreamId::try_new(format!("{COMPONENT}/out"))?,
            );
        let seen = [
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        ];
        for (index, name) in ["left", "right"].iter().enumerate() {
            let resource = ResourceId::try_new(*name)?;
            let owner = ComponentId::try_new(
                [
                    "00000000-0000-0000-0000-0000000002e7",
                    "00000000-0000-0000-0000-0000000001d1",
                ][index],
            )?;
            let sink = Sink {
                descriptor: ComponentDescriptor::try_new(
                    owner.clone(),
                    vec![port("in", PortDirection::Input)?],
                )?,
                received: seen[index].clone(),
                notification: Arc::new(Notify::new()),
            };
            let sink: Box<dyn EnvelopeSink> = match &flow {
                Some(flow) => Box::new(HeldSink {
                    sink,
                    entered: flow.entered[index].clone(),
                    release: Some(flow.release[index].clone()),
                }),
                None => Box::new(sink),
            };
            graph = graph
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Graph,
                    binding: (*name).into(),
                })?
                .provide_resource(resource.clone(), channels[index].resource())?
                .sink(ComponentName::try_new(*name)?, sink)
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(component_id()?, PortId::try_new("out")?),
                        Endpoint::new(owner, PortId::try_new("in")?),
                    ),
                    Box::new(
                        channels[index]
                            .definition()
                            .pipe(resource, "target")
                            .with_shared_storage(storage.clone()),
                    ),
                );
        }
        Ok(Self {
            graph: graph.build()?,
            _group: group,
            channels,
            seen,
            progress,
            finish,
        })
    }
}

async fn backpressure() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let flow = Arc::new(FlowGate {
        entered: [Arc::new(Notify::new()), Arc::new(Notify::new())],
        release: [Arc::new(Notify::new()), Arc::new(Notify::new())],
    });
    let mut fixture =
        SharedFixture::with_flow(directory.path(), None, None, 1, Some(flow.clone())).await?;
    let mut progress = fixture.progress.subscribe();
    let run = fixture.graph.start()?;
    let control = run.control();
    let (result, inspected) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(run, async {
            let inspected = async {
                tokio::join!(flow.entered[0].notified(), flow.entered[1].notified());
                let key = SourceProgressKey::Stream(StreamId::try_new("source/out")?);
                assert_eq!(fixture.progress.snapshot().checkpoints[&key].sequence, 1);
                let waiting = tokio::time::timeout(Duration::from_millis(30), async {
                    while progress
                        .borrow_and_update()
                        .checkpoints
                        .get(&key)
                        .is_none_or(|value| value.sequence < 2)
                    {
                        progress.changed().await?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await;
                assert!(
                    waiting.is_err(),
                    "full shared destinations did not backpressure input"
                );
                let snapshot = OutputCatalogue::new(control.clone())
                    .resolve(&OutputEndpoint::new(
                        component_id()?,
                        PortId::try_new("out")?,
                    ))?
                    .snapshot()
                    .await?;
                assert_eq!(snapshot.cursor().sequence(), 1);
                for release in &flow.release {
                    release.notify_one();
                }
                while progress
                    .borrow_and_update()
                    .checkpoints
                    .get(&key)
                    .is_none_or(|value| value.sequence < 2)
                {
                    progress.changed().await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            for release in &flow.release {
                release.notify_one();
            }
            fixture.finish.notify_one();
            inspected
        })
    })
    .await?;
    let stopped = fixture.graph.shutdown().await;
    inspected?;
    result?;
    stopped?;
    for seen in &fixture.seen {
        let rows = seen.lock().expect("shared result capture poisoned");
        assert_eq!(rows.len(), 2);
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: rows[1].clone(),
            }],
            Some(10.0),
            11.0,
            2,
        )?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_shared_backpressure_does_not_hold_commit_gate_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), backpressure()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_shared_backpressure_does_not_hold_commit_gate_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), backpressure()).await?
}

async fn shared_output() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut fixture = SharedFixture::new(directory.path(), None, None, 1).await?;
    let mut progress = fixture.progress.subscribe();
    let run = fixture.graph.start()?;
    let (result, inspected) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(run, async {
            let inspected = async {
                let key = SourceProgressKey::Stream(StreamId::try_new("source/out")?);
                while !progress.borrow_and_update().checkpoints.contains_key(&key) {
                    progress.changed().await?;
                }
                assert_eq!(fixture.progress.snapshot().checkpoints[&key].sequence, 1);
                for channel in &fixture.channels {
                    assert_eq!(
                        channel.progress().await?.accepted,
                        1,
                        "input progress escaped before shared journal cache publication"
                    );
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            fixture.finish.notify_one();
            inspected
        })
    })
    .await?;
    if result.is_ok() && inspected.is_ok() {
        for (channel, seen) in fixture.channels.iter().zip(&fixture.seen) {
            assert_eq!(channel.progress().await?.accepted, 1);
            assert_eq!(channel.progress().await?.processed["target"], 1);
            let rows = seen.lock().expect("shared result capture poisoned");
            assert_eq!(rows.len(), 1);
            aggregate(
                &[OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: rows[0].clone(),
                }],
                Some(0.0),
                10.0,
                1,
            )?;
        }
    }
    let stopped = fixture.graph.shutdown().await;
    inspected?;
    result?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "process-exit fixture launched by the shared commit-boundary cases"]
async fn indexed_shared_exit_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(SHARED_ROOT).context("missing shared crash root")?);
    let phase = std::env::var(SHARED_PHASE)?;
    anyhow::ensure!(
        phase == "before" || phase == "after",
        "unknown shared crash phase"
    );
    let gate = Arc::new(CommitGate {
        after: phase == "after",
        require_output: true,
        ..Default::default()
    });
    let release = Arc::new(Notify::new());
    let mut fixture =
        SharedFixture::new(&root, Some(gate.clone()), Some(release.clone()), 1).await?;
    let run = fixture.graph.start()?;
    let control = run.control();
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => anyhow::bail!("shared processing ended before the commit barrier: {result:?}"),
        result = async {
            assert_eq!(control.startup_report().await?.summary, OperationSummary::Completed);
            gate.armed.store(true, Ordering::Release);
            release.notify_one();
            gate.entered.notified().await;
            assert!(fixture.seen.iter().all(|seen| seen.lock().expect("shared result capture poisoned").is_empty()));
            assert!(fixture.progress.snapshot().checkpoints.is_empty(),
                "progress escaped before the actual shared commit was confirmed");
            Ok::<_, anyhow::Error>(())
        } => result?,
    }
    std::process::exit(if phase == "before" { 91 } else { 92 })
}

async fn shared_crash(phase: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let child = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable::destinations::shared::indexed_shared_exit_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SHARED_ROOT, directory.path())
            .env(SHARED_PHASE, phase)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        child.status.code() == Some(if phase == "before" { 91 } else { 92 }),
        "shared commit child failed: {}\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let mut fixture = SharedFixture::new(directory.path(), None, None, 1).await?;
    for channel in &fixture.channels {
        assert_eq!(
            channel.progress().await?.accepted,
            u64::from(phase == "after"),
            "shared journals did not recover the same physical commit boundary"
        );
    }
    fixture.finish.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(15), fixture.graph.start()?).await;
    if result.as_ref().is_ok_and(|result| result.is_ok()) {
        for (channel, seen) in fixture.channels.iter().zip(&fixture.seen) {
            assert_eq!(channel.progress().await?.accepted, 1);
            assert_eq!(channel.progress().await?.processed["target"], 1);
            let rows = seen.lock().expect("shared result capture poisoned");
            assert_eq!(rows.len(), 1);
            aggregate(
                &[OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: rows[0].clone(),
                }],
                Some(0.0),
                10.0,
                1,
            )?;
        }
    }
    let stopped = fixture.graph.shutdown().await;
    result??;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_shared_before_commit_exit_recovers_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(50), shared_crash("before")).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_shared_before_commit_exit_recovers_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(50), shared_crash("before")).await?
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_shared_after_commit_exit_recovers_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(50), shared_crash("after")).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_shared_after_commit_exit_recovers_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(50), shared_crash("after")).await?
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_output_uses_actual_shared_journal_transaction_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), shared_output()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_output_uses_actual_shared_journal_transaction_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), shared_output()).await?
}
