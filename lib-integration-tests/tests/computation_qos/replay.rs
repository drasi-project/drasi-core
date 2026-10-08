// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[path = "shared.rs"]
mod shared;

pub(super) fn options() -> ReplayOptions {
    ReplayOptions {
        failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
        receipt_capacity: NonZeroUsize::new(2).expect("window"),
    }
}

fn codec() -> Result<EnvelopeCodec> {
    Ok(FactoryRegistry::standard().envelope_codec(NonZeroUsize::new(1 << 20).expect("limit"))?)
}

pub(super) async fn admitted_output(path: &Path) -> Result<ChangeEnvelope> {
    let definition = definition(true, RetentionPolicy::Backpressure, 2);
    let source = persistent(path, definition.clone()).await?;
    source.enable_admission(admission_options()?).await?;
    assert!(source.enable_replay(options()).await.is_err());
    let session = source
        .register_producer(ComponentId::try_new("client")?)
        .await?;
    source.admit(&session, 1, &event(1)?).await?;
    let mut connection = endpoint(&source, &definition, "fast", false)?;
    let mut receiver = connection.pipe.take_receiver()?;
    let output = receiver
        .receive()
        .await?
        .expect("admitted output")
        .envelope()
        .clone();
    connection.control.cancel();
    source.shutdown().await?;
    Ok(output)
}

#[tokio::test]
async fn interrupted_output_replay_commits_content_and_receipt_together() -> Result<()> {
    for cancel in [false, true] {
        for before_commit in [false, true] {
            let directory = tempfile::tempdir()?;
            let input = admitted_output(&directory.path().join("source")).await?;
            let path = directory.path().join("output");
            let definition = definition(true, RetentionPolicy::Backpressure, 2);
            let fail = Arc::new(AtomicBool::new(false));
            let gate = Arc::new(CommitGate {
                before_commit,
                entered: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            });
            let channel =
                uncertain_channel(&path, definition.clone(), fail.clone(), Some(gate.clone()))
                    .await?;
            channel.enable_replay(options()).await?;
            let connection = endpoint(&channel, &definition, "fast", false)?;
            let sender = connection.pipe.sender();
            fail.store(true, Ordering::Release);
            let mut operation = Box::pin(sender.send(input.clone()));
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::select! {
                    _ = gate.entered.notified() => {},
                    result = &mut operation => panic!("commit was not interrupted: {result:?}"),
                }
            })
            .await?;
            if cancel {
                drop(operation);
                assert!(channel.progress().await.is_err());
            } else {
                connection.control.cancel();
                gate.resume.notify_one();
                assert!(matches!(
                    operation.await.expect_err("revoked send").error,
                    PipeError::AcceptanceUnknown { .. }
                ));
            }
            channel.shutdown().await?;
            let restored = persistent(&path, definition.clone()).await?;
            assert_eq!(
                restored.progress().await?.accepted,
                u64::from(!cancel || !before_commit)
            );
            assert_eq!(restored.publish(&input).await?.position(), Some(1));
            assert_eq!(restored.progress().await?.accepted, 1);
            handle_admitted(&restored, &definition, 1).await?;
            restored.shutdown().await?;
        }
    }
    Ok(())
}

const CRASH_ROOT: &str = "DRASI_QOS_REPLAY_CRASH_ROOT";
const CRASH_BEFORE: &str = "DRASI_QOS_REPLAY_CRASH_BEFORE";

#[tokio::test]
async fn output_binding_cannot_observe_uncommitted_replay_configuration() -> Result<()> {
    for before_commit in [false, true] {
        let directory = tempfile::tempdir()?;
        let definition = definition(true, RetentionPolicy::Backpressure, 2);
        let fail = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(CommitGate {
            before_commit,
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let channel = uncertain_channel(
            directory.path(),
            definition.clone(),
            fail.clone(),
            Some(gate.clone()),
        )
        .await?;
        fail.store(true, Ordering::Release);
        let mut configure = Box::pin(channel.enable_replay(options()));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = gate.entered.notified() => {},
                result = &mut configure => panic!("configuration did not pause: {result:?}"),
            }
        })
        .await?;
        assert_eq!(channel.output_journal_identity(), None);
        assert!(endpoint(&channel, &definition, "fast", false).is_err());
        drop(configure);
        assert!(endpoint(&channel, &definition, "fast", false).is_err());
        channel.shutdown().await?;
        drop(channel);
        let restored = persistent(directory.path(), definition.clone()).await?;
        assert_eq!(restored.output_journal_identity().is_some(), !before_commit);
        restored.enable_replay(options()).await?;
        let binding = endpoint(&restored, &definition, "fast", false)?;
        binding.control.cancel();
        restored.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "invoked by output_replay_survives_real_process_exit_at_commit"]
async fn output_replay_crash_child() -> Result<()> {
    let path = std::path::PathBuf::from(std::env::var(CRASH_ROOT)?);
    let before_commit = std::env::var(CRASH_BEFORE)? == "true";
    let input = codec()?.decode(&std::fs::read(path.join("input.json"))?)?;
    let gate = Arc::new(CommitGate {
        before_commit,
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let channel = uncertain_channel(
        &path.join("output"),
        definition(true, RetentionPolicy::Backpressure, 2),
        Arc::new(AtomicBool::new(true)),
        Some(gate.clone()),
    )
    .await?;
    let mut operation = Box::pin(channel.publish(&input));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = gate.entered.notified() => {},
            result = &mut operation => panic!("commit was not reached: {result:?}"),
        }
    })
    .await?;
    std::process::exit(75);
}

#[tokio::test]
async fn output_replay_survives_real_process_exit_at_commit() -> Result<()> {
    use std::process::{Command, Stdio};
    for before_commit in [false, true] {
        let directory = tempfile::tempdir()?;
        let input = admitted_output(&directory.path().join("source")).await?;
        std::fs::write(
            directory.path().join("input.json"),
            codec()?.encode(&input)?,
        )?;
        let definition = definition(true, RetentionPolicy::Backpressure, 2);
        let channel = persistent(&directory.path().join("output"), definition.clone()).await?;
        channel.enable_replay(options()).await?;
        channel.shutdown().await?;
        drop(channel);
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "replay::output_replay_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CRASH_ROOT, directory.path())
            .env(CRASH_BEFORE, before_commit.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while child.try_wait()?.is_none() {
            if std::time::Instant::now() > deadline {
                child.kill()?;
                let output = child.wait_with_output()?;
                panic!("crash child timed out: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        assert_eq!(output.status.code(), Some(75), "{output:?}");
        let channel = persistent(&directory.path().join("output"), definition.clone()).await?;
        assert_eq!(
            channel.progress().await?.accepted,
            u64::from(!before_commit)
        );
        assert_eq!(channel.publish(&input).await?.position(), Some(1));
        assert_eq!(channel.progress().await?.accepted, 1);
        handle_admitted(&channel, &definition, 1).await?;
        channel.shutdown().await?;
    }
    Ok(())
}

async fn query(path: &Path) -> Result<TransactionTransformer> {
    let mut query =
        TransactionTransformer::from_query(unstarted_query(path, QueryOptions::default()).await?);
    query.start().await?;
    Ok(query)
}

async fn unstarted_query(path: &Path, options: QueryOptions) -> Result<ContinuousQueryTransformer> {
    let provider =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)));
    ContinuousQueryTransformer::new_with_options(
        ContinuousQueryDefinition {
            graph_id: "query-graph".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Item) RETURN n".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).expect("window"),
        },
        provider,
        options,
    )
    .await
}

async fn output_channel(path: &Path, definition: QosChannelDefinition) -> Result<Arc<QosChannel>> {
    let provider =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)));
    let channel = QosChannel::persistent(
        definition,
        provider.create_indexes("qos", "channel").await?,
        codec()?,
        "journal",
    )
    .await?;
    channel.enable_replay(options()).await?;
    Ok(channel)
}

async fn handle_query(channel: &Arc<QosChannel>, definition: &QosChannelDefinition) -> Result<()> {
    for subscriber in ["fast", "slow"] {
        let mut connection = endpoint(channel, definition, subscriber, false)?;
        let mut receiver = connection.pipe.take_receiver()?;
        let delivery = receiver.receive().await?.expect("query result");
        assert_eq!(QueryChangeCodec::query_sequence(delivery.envelope())?, 1);
        assert_eq!(
            QueryChangeCodec::decode_evaluation(delivery.envelope())?.len(),
            1
        );
        delivery
            .into_parts()
            .1
            .expect("ack")
            .complete(HandlingOutcome::Handled)
            .await?;
        connection.control.cancel();
    }
    Ok(())
}

#[tokio::test]
async fn retained_query_output_recovers_partial_separate_store_handoff_without_duplicate_append(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut definition = definition(true, RetentionPolicy::Backpressure, 2);
    definition.stream = StreamId::try_new("query/out")?;
    let mut producer = query(&directory.path().join("producer")).await?;
    let left = output_channel(&directory.path().join("left"), definition.clone()).await?;
    let right = output_channel(&directory.path().join("right"), definition.clone()).await?;
    let bindings =
        OutputBindings::try_new([("left", &left), ("right", &right)].into_iter().flat_map(
            |(side, channel)| {
                ["fast", "slow"].map(move |subscriber| OutputDestination {
                    output: PortId::try_new("out").unwrap(),
                    consumer: ComponentId::try_new(format!("{side}-{subscriber}")).unwrap(),
                    input: PortId::try_new("in").unwrap(),
                    journal: channel.output_journal_identity().unwrap(),
                    subscriber: subscriber.into(),
                })
            },
        ))?;
    producer.bind_output_destinations(&bindings).await?;
    let output = producer
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: event(1)?,
        })
        .await?;
    assert_eq!(output.len(), 1);
    assert_eq!(left.publish(&output[0].envelope).await?.position(), Some(1));
    handle_query(&left, &definition).await?;
    assert_eq!(right.progress().await?.accepted, 0);
    producer.bind_output_destinations(&bindings).await?;
    assert!(producer.has_pending_emissions());
    let resumed = producer.on_wakeup().await?;
    assert_eq!(QueryChangeCodec::query_sequence(&resumed[0].envelope)?, 1);
    assert_ne!(resumed[0].envelope.id(), output[0].envelope.id());
    assert_eq!(
        left.publish(&resumed[0].envelope).await?.position(),
        Some(1)
    );
    producer.stop().await?;
    left.shutdown().await?;
    right.shutdown().await?;
    drop((producer, left, right));

    let mut producer = query(&directory.path().join("producer")).await?;
    let left = output_channel(&directory.path().join("left"), definition.clone()).await?;
    let right = output_channel(&directory.path().join("right"), definition.clone()).await?;
    assert!(producer
        .bind_output_destinations(&OutputBindings::default())
        .await
        .is_err());
    producer.bind_output_destinations(&bindings).await?;
    let replayed = producer.on_wakeup().await?;
    assert_eq!(replayed.len(), 1);
    assert_ne!(output[0].envelope.id(), replayed[0].envelope.id());
    assert_eq!(QueryChangeCodec::query_sequence(&replayed[0].envelope)?, 1);
    assert_eq!(
        left.publish(&replayed[0].envelope).await?.position(),
        Some(1)
    );
    assert_eq!(
        right.publish(&replayed[0].envelope).await?.position(),
        Some(1)
    );
    assert_eq!(left.progress().await?.accepted, 1);
    assert_eq!(left.progress().await?.processed["fast"], 1);
    handle_query(&right, &definition).await?;
    producer.delivery_completed(&replayed).await?;
    producer
        .bind_output_destinations(&OutputBindings::default())
        .await?;
    producer.stop().await?;
    left.shutdown().await?;
    right.shutdown().await?;
    drop((producer, left, right));

    let mut producer = query(&directory.path().join("producer")).await?;
    assert!(
        !producer
            .wakeup_source()
            .expect("query wakeups")
            .has_pending()
            .await?
    );
    producer.stop().await?;
    Ok(())
}

struct BindingBootstrap(Arc<AtomicUsize>);

#[async_trait]
impl ComputationBootstrapProvider for BindingBootstrap {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::empty()),
            watermarks: Vec::new(),
        })
    }
}

#[tokio::test]
async fn query_output_bindings_cannot_be_reset_or_reopened_without_tracking() -> Result<()> {
    const MEMBERSHIP: &str = "\0computation:output-bindings:v1";
    const DELIVERED: &str = "\0computation:query-delivered:v1";
    for corruption in [
        "missing-membership",
        "missing-fingerprint",
        "changed-membership",
        "changed-fingerprint",
        "missing-progress",
        "disabled-tracking",
        "automatic-reset",
    ] {
        let directory = tempfile::tempdir()?;
        let bindings = OutputBindings::try_new([OutputDestination {
            output: PortId::try_new("out")?,
            consumer: ComponentId::try_new("consumer")?,
            input: PortId::try_new("in")?,
            journal: "00000000-0000-0000-0000-000000000001".parse()?,
            subscriber: "subscriber".into(),
        }])?;
        {
            let mut producer = query(directory.path()).await?;
            producer.bind_output_destinations(&bindings).await?;
            producer
                .transform(InputEnvelope {
                    port: PortId::try_new("in")?,
                    envelope: event(1)?,
                })
                .await?;
            producer.stop().await?;
        }
        let provider = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
            directory.path(),
            false,
            false,
        )));
        let indexes = provider.create_indexes("query-graph", "query").await?;
        let checkpoint = indexes.checkpoint_store().expect("checkpoints").clone();
        let mut saved = checkpoint.read_all_checkpoints().await?;
        assert_eq!(
            saved[DELIVERED]
                .source_position
                .as_ref()
                .expect("fingerprint")
                .len(),
            32
        );
        match corruption {
            "missing-membership" => {
                saved.remove(MEMBERSHIP);
            }
            "missing-progress" => {
                saved.remove(DELIVERED);
            }
            "missing-fingerprint" => saved.get_mut(DELIVERED).unwrap().source_position = None,
            "changed-fingerprint" => {
                saved.get_mut(DELIVERED).unwrap().source_position =
                    Some(Bytes::from_static(b"wrong"))
            }
            "changed-membership" => {
                let record = saved.get_mut(MEMBERSHIP).unwrap();
                let mut content: serde_json::Value =
                    serde_json::from_slice(record.source_position.as_ref().unwrap())?;
                content["destinations"][0]["consumer"] = "different".into();
                record.source_position = Some(serde_json::to_vec(&content)?.into());
            }
            "disabled-tracking" | "automatic-reset" => {}
            _ => unreachable!(),
        }
        if corruption != "disabled-tracking" {
            saved.insert(
                "\0computation:query-bootstrap:v1".into(),
                drasi_core::interface::SourceCheckpoint::new(0, None),
            );
        }
        let owner = drasi_core::computation::ComputationTransaction::try_new(indexes)?;
        owner
            .run(async {
                checkpoint.clear_checkpoints().await?;
                for (key, record) in &saved {
                    checkpoint
                        .stage_checkpoint(key, record.sequence, record.source_position.as_ref())
                        .await?;
                }
                Ok(())
            })
            .await?;
        owner.shutdown().await?;
        drop((owner, checkpoint));

        let calls = Arc::new(AtomicUsize::new(0));
        let query = unstarted_query(
            directory.path(),
            QueryOptions {
                recovery: QueryRecoveryPolicy::AutoReset,
                publication: QueryPublicationMode::Atomic,
            },
        )
        .await?
        .with_bootstrap(Arc::new(BindingBootstrap(calls.clone())));
        let mut producer: Box<dyn Transformer> = if corruption == "disabled-tracking" {
            Box::new(query)
        } else {
            Box::new(TransactionTransformer::from_query(query))
        };
        if corruption != "disabled-tracking" {
            let error = producer
                .bind_output_destinations(&bindings)
                .await
                .expect_err("not recovered");
            assert_eq!(
                error.downcast_ref::<OutputBindingError>(),
                Some(&OutputBindingError::RecoveryRequired)
            );
        }
        let error = producer.start().await.expect_err(corruption);
        assert!(
            error.downcast_ref::<OutputBindingError>().is_some(),
            "{corruption}: {error:#}"
        );
        if corruption == "automatic-reset" {
            assert_eq!(
                error.downcast_ref::<OutputBindingError>(),
                Some(&OutputBindingError::ResetRequiresUnboundDestinations)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{corruption}");
        if corruption != "disabled-tracking" {
            let error = producer
                .bind_output_destinations(&bindings)
                .await
                .expect_err("failed recovery");
            assert_eq!(
                error.downcast_ref::<OutputBindingError>(),
                Some(&OutputBindingError::RecoveryRequired)
            );
        }
        producer.stop().await?;
        drop(producer);
        let indexes = provider.create_indexes("query-graph", "query").await?;
        let actual = indexes
            .checkpoint_store()
            .unwrap()
            .read_all_checkpoints()
            .await?;
        assert_eq!(actual.len(), saved.len(), "{corruption}");
        for (key, record) in saved {
            assert_eq!(
                actual[&key].sequence, record.sequence,
                "{corruption}: {key}"
            );
            assert_eq!(
                actual[&key].source_position, record.source_position,
                "{corruption}: {key}"
            );
        }
        let owner = drasi_core::computation::ComputationTransaction::try_new(indexes)?;
        owner.shutdown().await?;
    }
    Ok(())
}

struct BindingSource {
    descriptor: ComponentDescriptor,
    events: std::collections::VecDeque<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for BindingSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for BindingSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.events.pop_front().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("output port"),
            envelope,
        }))
    }
}

#[derive(Default)]
struct BindingObservations {
    rows: std::sync::Mutex<Vec<(String, String, u64)>>,
    changed: tokio::sync::Notify,
}

struct BindingSink {
    descriptor: ComponentDescriptor,
    destination_url: String,
    seen: Arc<BindingObservations>,
}

#[async_trait]
impl ComputationComponent for BindingSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"url": self.destination_url}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for BindingSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.seen.rows.lock().expect("observations").push((
            self.descriptor.id().to_string(),
            self.destination_url.clone(),
            QueryChangeCodec::query_sequence(&input.envelope)?,
        ));
        self.seen.changed.notify_one();
        Ok(())
    }
}

async fn binding_graph(
    producer_path: &Path,
    channels: &[Arc<QosChannel>; 2],
    fresh: bool,
    url: &str,
    seen: Arc<BindingObservations>,
) -> Result<ComputationGraph> {
    let ep = |component: &str, port: &str| -> Result<Endpoint> {
        Ok(Endpoint::new(
            ComponentId::try_new(component)?,
            PortId::try_new(port)?,
        ))
    };
    let describe =
        |id: &str, port: &str, direction, schema: &Schema| -> Result<ComponentDescriptor> {
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
    let producer = query(producer_path).await?;
    let mut builder = ComputationGraph::builder("query-graph")
        .source(Box::new(BindingSource {
            descriptor: describe(
                "source",
                "out",
                PortDirection::Output,
                &GraphChangeCodec::schema(),
            )?,
            events: if fresh {
                [event(1)?].into()
            } else {
                Default::default()
            },
        }))
        .transformer(Box::new(producer))
        .bind_stream(ep("source", "out")?, StreamId::try_new("source/out")?)
        .bind_stream(ep("query", "out")?, StreamId::try_new("query/out")?)
        .connect(
            EdgeDefinition::new(ep("source", "out")?, ep("query", "in")?),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
    for (side, channel) in ["left", "right"].into_iter().zip(channels) {
        let resource = ResourceId::try_new(side)?;
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: side.into(),
            })?
            .provide_resource(resource.clone(), channel.resource())?
            .sink(Box::new(BindingSink {
                descriptor: describe(
                    side,
                    "in",
                    PortDirection::Input,
                    &QueryChangeCodec::schema(),
                )?,
                destination_url: url.into(),
                seen: seen.clone(),
            }))
            .connect(
                EdgeDefinition::new(ep("query", "out")?, ep(side, "in")?),
                Box::new(channel.definition().pipe(resource, "target")),
            );
    }
    Ok(builder.build()?)
}

#[tokio::test]
async fn graph_pending_output_follows_replacement_but_rejects_a_different_journal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut definition = definition(true, RetentionPolicy::Backpressure, 2);
    definition.stream = StreamId::try_new("query/out")?;
    definition.subscribers = BTreeMap::from([("target".into(), SubscriptionStart::Earliest)]);
    let left = output_channel(&directory.path().join("left"), definition.clone()).await?;
    let fail = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(CommitGate {
        before_commit: true,
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let right = uncertain_channel(
        &directory.path().join("right"),
        definition.clone(),
        fail.clone(),
        Some(gate.clone()),
    )
    .await?;
    right.enable_replay(options()).await?;
    let replacement = output_channel(
        &directory.path().join("different-right"),
        definition.clone(),
    )
    .await?;
    let seen = Arc::new(BindingObservations::default());
    let channels = [left.clone(), right.clone()];
    let mut graph = binding_graph(
        &directory.path().join("producer"),
        &channels,
        true,
        "old-url",
        seen.clone(),
    )
    .await?;
    fail.store(true, Ordering::Release);
    let run = graph.start()?;
    let control = run.control();
    let (result, inspected) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(run, async {
            assert_eq!(
                control.startup_report().await?.summary,
                OperationSummary::Completed
            );
            gate.entered.notified().await;
            while seen.rows.lock().unwrap().is_empty() {
                seen.changed.notified().await;
            }
            let revision = control.desired_snapshot().revision;
            let resource = ResourceId::try_new("right")?;
            let preview = control
                .preview(
                    revision,
                    vec![DesiredMutation::RebindResource(resource.clone())],
                )
                .await?;
            let mut replacements = TopologyBindings::default();
            replacements
                .resources
                .insert(resource, replacement.resource());
            let error = control
                .reconcile(preview, replacements)
                .await
                .expect_err("cannot replace a pending journal");
            let GraphError::Component { source, .. } = error else {
                panic!("unexpected rejection: {error:?}");
            };
            assert_eq!(
                source.downcast_ref::<OutputBindingError>(),
                Some(&OutputBindingError::PendingDestinationsChanged)
            );
            assert_eq!(control.desired_snapshot().revision, revision);
            control.cancel();
            Ok::<_, anyhow::Error>(())
        })
    })
    .await?;
    inspected?;
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    graph.shutdown().await?;
    left.shutdown().await?;
    right.shutdown().await?;
    replacement.shutdown().await?;
    drop((graph, channels, left, right, replacement));
    assert_eq!(
        *seen.rows.lock().unwrap(),
        [("left".into(), "old-url".into(), 1)]
    );

    let left = output_channel(&directory.path().join("left"), definition.clone()).await?;
    let right = output_channel(&directory.path().join("right"), definition).await?;
    let mut graph = binding_graph(
        &directory.path().join("producer"),
        &[left.clone(), right.clone()],
        false,
        "new-url",
        seen.clone(),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(20), graph.start()?).await??;
    assert_eq!(
        *seen.rows.lock().unwrap(),
        [
            ("left".into(), "old-url".into(), 1),
            ("right".into(), "new-url".into(), 1),
        ]
    );
    assert_eq!(left.progress().await?.accepted, 1);
    assert_eq!(right.progress().await?.accepted, 1);
    graph.shutdown().await?;
    left.shutdown().await?;
    right.shutdown().await?;
    drop((graph, left, right));
    let mut producer = query(&directory.path().join("producer")).await?;
    assert!(!producer.wakeup_source().unwrap().has_pending().await?);
    producer.stop().await?;
    Ok(())
}
