// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{ComputationIndexes, ComputationResource, TransactionDomain},
    interface::{IndexError, IndexSet, SessionControl},
};

struct Boundary {
    remaining: AtomicUsize,
    after: bool,
    cancel: bool,
    entered: tokio::sync::Notify,
    /// After the boundary is hit, every commit fails until this is cleared
    /// (a storage outage rather than one failed write).
    outage: std::sync::atomic::AtomicBool,
    sustain: bool,
}
struct FaultControl {
    inner: Arc<dyn SessionControl>,
    boundary: Arc<Boundary>,
}
#[async_trait]
impl SessionControl for FaultControl {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), IndexError> {
        if self.boundary.outage.load(Ordering::Acquire) {
            return Err(IndexError::IOError);
        }
        let hit = self.boundary.remaining.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |remaining| remaining.checked_sub(1),
        ) == Ok(1);
        if !hit || self.boundary.after {
            self.inner.commit().await?;
        }
        if hit {
            if self.boundary.sustain {
                self.boundary.outage.store(true, Ordering::Release);
            }
            self.boundary.entered.notify_one();
            if self.boundary.cancel {
                std::future::pending::<()>().await;
            }
            return Err(IndexError::IOError);
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.inner.rollback()
    }
}
struct FaultProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    boundary: Arc<Boundary>,
}
#[async_trait]
impl ComputationIndexProvider for FaultProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &drasi_core_types::ComponentId,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, component).await?;
        let control: Arc<dyn SessionControl> = Arc::new(FaultControl {
            inner: original.indexes().session_control.clone(),
            boundary: self.boundary.clone(),
        });
        let domain = TransactionDomain::new(control.clone());
        let set = original.indexes();
        Ok(ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original.checkpoint_store().unwrap().clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().unwrap().clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.live_results_writer().unwrap().clone(),
                &domain,
            )),
        )
        .unwrap()
        .with_cleanup(original.cleanup().unwrap().clone())
        .with_durability(original.durability()))
    }
    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "required child of native_consumer_completion_recovers_after_process_exit"]
async fn native_consumer_completion_crash_worker() -> anyhow::Result<()> {
    let directory = std::path::PathBuf::from(std::env::var("DRASI_CONSUMER_CRASH_DIRECTORY")?);
    let mode = match std::env::var("DRASI_CONSUMER_CRASH_MODE")?.as_str() {
        "external" => sdk::ConsumerMode::External,
        "transactional" => sdk::ConsumerMode::Transactional,
        _ => anyhow::bail!("unknown crash mode"),
    };
    let after: bool = std::env::var("DRASI_CONSUMER_CRASH_AFTER")?.parse()?;
    let plugin = super::super::super::factory::recovery_tests::sqlite_plugin()?;
    let boundary = Arc::new(Boundary {
        remaining: AtomicUsize::new(0),
        after,
        cancel: true,
        entered: tokio::sync::Notify::new(),
        outage: std::sync::atomic::AtomicBool::new(false),
        sustain: false,
    });
    let factory = factory(&plugin, mode);
    let mut consumer = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({"path":directory.join("effects.db")}),
        scope(),
        Arc::new(FaultProvider {
            inner: provider(&directory.join("progress"))?,
            boundary: boundary.clone(),
        }),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    boundary.remaining.store(2, Ordering::Release);
    let operation = consumer.handle(batch()?);
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => anyhow::bail!("completion did not pause: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), boundary.entered.notified()) => {
            result?;
            std::process::exit(if after { 87 } else { 86 });
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_completion_recovers_after_process_exit() -> anyhow::Result<()> {
    let plugin = super::super::super::factory::recovery_tests::sqlite_plugin()?;
    for mode in [
        sdk::ConsumerMode::External,
        sdk::ConsumerMode::Transactional,
    ] {
        for after in [false, true] {
            let directory = tempfile::tempdir()?;
            let mut child = tokio::process::Command::new(std::env::current_exe()?);
            child
                .args([
                    "--exact",
                    concat!(module_path!(), "::native_consumer_completion_crash_worker")
                        .strip_prefix("drasi_host_sdk::")
                        .unwrap(),
                    "--ignored",
                ])
                .env("DRASI_CONSUMER_CRASH_DIRECTORY", directory.path())
                .env(
                    "DRASI_CONSUMER_CRASH_MODE",
                    if mode == sdk::ConsumerMode::External {
                        "external"
                    } else {
                        "transactional"
                    },
                )
                .env("DRASI_CONSUMER_CRASH_AFTER", after.to_string())
                .kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(30), child.output()).await??;
            assert_eq!(
                output.status.code(),
                Some(if after { 87 } else { 86 }),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let provider = provider(&directory.path().join("progress"))?;
            if mode == sdk::ConsumerMode::Transactional {
                assert_eq!(
                    count(provider.as_ref()).await?,
                    after.then_some(ElementValue::Integer(1))
                );
            } else {
                assert_eq!(effects(&directory.path().join("effects.db"))?, (1, 1, 1));
            }
            let factory = factory(&plugin, mode);
            let mut consumer = initialized_consumer(
                factory.as_ref(),
                id(423),
                json!({"path":directory.path().join("effects.db")}),
                sdk::Scope {
                    generation: 2,
                    ..scope()
                },
                provider.clone(),
                options(),
            )
            .await?;
            consumer
                .start(&crate::computation::test_hooks::start_ctx())
                .await?;
            assert_eq!(
                consumer.progress().await?[0].completed_operations,
                usize::from(after)
            );
            consumer.handle(batch()?).await?;
            assert_eq!(consumer.progress().await?[0].handled_sequence, 1);
            consumer.handle(batch()?).await?;
            consumer
                .stop(&crate::computation::test_hooks::stop_ctx())
                .await?;
            if mode == sdk::ConsumerMode::Transactional {
                assert_eq!(
                    count(provider.as_ref()).await?,
                    Some(ElementValue::Integer(3))
                );
            } else {
                assert_eq!(
                    effects(&directory.path().join("effects.db"))?,
                    (3, if after { 3 } else { 4 }, 3)
                );
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_real_library_recovers_uncertain_completion_without_losing_operations(
) -> anyhow::Result<()> {
    let plugin = super::super::super::factory::recovery_tests::sqlite_plugin()?;
    for mode in [
        sdk::ConsumerMode::External,
        sdk::ConsumerMode::Transactional,
    ] {
        for after in [false, true] {
            for cancel in [false, true] {
                let directory = tempfile::tempdir()?;
                let path = directory.path().join("effects.db");
                let inner = provider(&directory.path().join("progress"))?;
                let boundary = Arc::new(Boundary {
                    remaining: AtomicUsize::new(0),
                    after,
                    cancel,
                    entered: tokio::sync::Notify::new(),
                    outage: std::sync::atomic::AtomicBool::new(false),
                    sustain: false,
                });
                let provider = Arc::new(FaultProvider {
                    inner: inner.clone(),
                    boundary: boundary.clone(),
                });
                let factory = factory(&plugin, mode);
                let mut consumer = initialized_consumer(
                    factory.as_ref(),
                    id(423),
                    json!({"path":path}),
                    scope(),
                    provider,
                    options(),
                )
                .await?;
                consumer
                    .start(&crate::computation::test_hooks::start_ctx())
                    .await?;
                // After startup, admission commits first, then operation completion.
                boundary.remaining.store(2, Ordering::Release);
                if cancel {
                    let handle = consumer.handle(batch()?);
                    tokio::pin!(handle);
                    tokio::select! {
                        result = &mut handle => panic!("commit boundary unexpectedly completed: {result:?}"),
                        entered = tokio::time::timeout(Duration::from_secs(10), boundary.entered.notified()) => { entered?; }
                    }
                } else {
                    assert!(consumer.handle(batch()?).await.is_err());
                }
                assert_eq!(boundary.remaining.load(Ordering::Acquire), 0);
                assert!(
                    consumer.progress().await.is_err(),
                    "uncertain commit must fence cached progress"
                );
                if !cancel {
                    // The failed delivery settled; retrying it reopens the
                    // fenced ledger from storage and resumes at the
                    // completed prefix within the same activation.
                    consumer.handle(batch()?).await?;
                    assert_eq!(consumer.progress().await?[0].handled_sequence, 1);
                    consumer
                        .stop(&crate::computation::test_hooks::stop_ctx())
                        .await?;
                    if mode == sdk::ConsumerMode::Transactional {
                        assert_eq!(count(inner.as_ref()).await?, Some(ElementValue::Integer(3)));
                    } else {
                        assert_eq!(effects(&path)?, (3, if after { 3 } else { 4 }, 3));
                    }
                    continue;
                }
                // A cancelled delivery may have left the plugin's batch open:
                // only a restart cleans it up.
                assert!(consumer.handle(batch()?).await.is_err());
                consumer
                    .stop(&crate::computation::test_hooks::stop_ctx())
                    .await?;
                if mode == sdk::ConsumerMode::Transactional {
                    assert_eq!(
                        count(inner.as_ref()).await?,
                        after.then_some(ElementValue::Integer(1))
                    );
                } else {
                    assert_eq!(effects(&path)?, (1, 1, 1));
                }
                let mut consumer = initialized_consumer(
                    factory.as_ref(),
                    id(423),
                    json!({"path":path}),
                    scope(),
                    inner.clone(),
                    options(),
                )
                .await?;
                consumer
                    .start(&crate::computation::test_hooks::start_ctx())
                    .await?;
                assert_eq!(
                    consumer.progress().await?[0].completed_operations,
                    usize::from(after)
                );
                consumer.handle(batch()?).await?;
                assert_eq!(consumer.progress().await?[0].handled_sequence, 1);
                consumer
                    .stop(&crate::computation::test_hooks::stop_ctx())
                    .await?;
                if mode == sdk::ConsumerMode::Transactional {
                    assert_eq!(count(inner.as_ref()).await?, Some(ElementValue::Integer(3)));
                } else {
                    assert_eq!(effects(&path)?, (3, if after { 3 } else { 4 }, 3));
                }
            }
        }
    }
    Ok(())
}

/// Emits each batch the test hands it, once.
struct HandedSource {
    descriptor: ComponentDescriptor,
    inputs: tokio::sync::mpsc::Receiver<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for HandedSource {
    /// Its batch carries a restart-stable producer identity.
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::new(
            drasi_lib::computation::v1::ProgressPersistence::Durable,
        )
        .with_outputs(
            &self.descriptor,
            drasi_lib::computation::v1::OutputDescription::DURABLE,
        )
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}
#[async_trait]
impl EnvelopeSource for HandedSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.inputs.recv().await.map(|envelope| OutputEnvelope {
            execution_context: Default::default(),
            port: PortId::try_new("out").unwrap(),
            envelope,
        }))
    }
}

/// RCA-7: the consumer-progress storage of a loaded native consumer starts
/// failing at a completion commit during live processing (the failed commit
/// applied or not) and stays down. The node reports a retryable `Processing`
/// failure without acknowledging its input upstream. Once storage recovers,
/// the graph's retry of the retained input resumes from the durable ledger in
/// the same generation: every operation reaches the destination exactly once
/// and the source never sends the input again.
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_storage_failure_is_retryable_and_resumes_without_loss(
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let plugin = super::super::super::factory::recovery_tests::sqlite_plugin()?;
    for mode in [
        sdk::ConsumerMode::External,
        sdk::ConsumerMode::Transactional,
    ] {
        for after in [false, true] {
            storage_failure_case(&plugin, mode, after)
                .await
                .with_context(|| format!("mode {mode:?}, commit applied {after}"))?;
        }
    }
    Ok(())
}

async fn observe_consumer(
    control: &GraphControl,
    consumer: &ComponentId,
    describe: &str,
    predicate: impl Fn(&ObservedComponent) -> bool,
) -> anyhow::Result<ObservedComponent> {
    use anyhow::Context as _;
    let mut changes = control.subscribe_observed();
    tokio::time::timeout(
        Duration::from_secs(10),
        changes.wait_for(|state| state.components.get(consumer).is_some_and(&predicate)),
    )
    .await
    .with_context(|| {
        format!(
            "consumer did not become {describe}: {:?}",
            control.observed().components.get(consumer)
        )
    })??;
    Ok(control.observed().components[consumer].clone())
}

async fn storage_failure_case(
    plugin: &Arc<NativePlugin>,
    mode: sdk::ConsumerMode,
    after: bool,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    const BOUND: Duration = Duration::from_secs(10);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("effects.db");
    let inner = provider(&directory.path().join("progress"))?;
    let boundary = Arc::new(Boundary {
        remaining: AtomicUsize::new(0),
        after,
        cancel: false,
        entered: tokio::sync::Notify::new(),
        outage: std::sync::atomic::AtomicBool::new(false),
        sustain: true,
    });
    let factory = factory(plugin, mode);
    let consumer = id(423);
    let mut specification = factory.specification(consumer.clone(), json!({"path": path}))?;
    let resource = ResourceId::try_new("delivery")?;
    specification
        .dependencies
        .insert("consumer".into(), vec![resource.clone()]);
    let (send, inputs) = tokio::sync::mpsc::channel(1);
    let mut graph = ComputationGraph::builder(GRAPH)
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Borrowed,
            binding: "delivery".into(),
        })?
        .provide_resource(
            resource,
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(NativeConsumerResource {
                    provider: Arc::new(FaultProvider {
                        inner: inner.clone(),
                        boundary: boundary.clone(),
                    }),
                    options: options(),
                }),
            ),
        )?
        .component(ComponentName::try_new("consumer")?, specification, factory)
        .source(
            ComponentName::try_new("source")?,
            Box::new(HandedSource {
                descriptor: ComponentDescriptor::try_new(
                    id(77),
                    vec![super::super::port(PortDirection::Output)],
                )?,
                inputs,
            }),
        )
        .bind_stream(
            Endpoint::new(id(77), PortId::try_new("out")?),
            StreamId::try_new("out")?,
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(id(77), PortId::try_new("out")?),
                Endpoint::new(consumer.clone(), PortId::try_new("in")?),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()?;
    let run = graph.run()?;
    let control = run.control();
    let scenario = async {
        let deployment = control.deployment_report().await?;
        anyhow::ensure!(
            deployment.summary == OperationSummary::Completed,
            "{deployment:?}"
        );
        control
            .start_components(deployment.revision, GraphSelection::All)
            .await?;
        let first = observe_consumer(&control, &consumer, "running", |node| {
            node.lifecycle == ComponentLifecycle::Running
        })
        .await?;
        // The runner records the input (commit 1), then the first operation's
        // completion (commit 2): the storage fails there and keeps failing.
        boundary.remaining.store(2, Ordering::Release);
        send.send(batch()?.envelope).await?;
        tokio::time::timeout(BOUND, boundary.entered.notified()).await?;
        let failed =
            observe_consumer(&control, &consumer, "failed", |node| node.failure.is_some()).await?;
        let failure = failed.failure.as_ref().expect("observed failure");
        assert_eq!(
            failure.disposition,
            FailureDisposition::Retryable,
            "{failure:?}"
        );
        assert_eq!(failure.phase, FailurePhase::Processing, "{failure:?}");
        assert_eq!(failed.generation, first.generation);
        assert_ne!(failed.health, ComponentHealth::Healthy);
        // Nothing beyond the operation in flight reached the destination, and
        // the graph keeps the unacknowledged input for its retry.
        match mode {
            sdk::ConsumerMode::External => {
                let (applied, _, distinct) = effects(&path)?;
                assert_eq!((applied, distinct), (1, 1));
            }
            _ => assert!(!matches!(
                count(inner.as_ref()).await,
                Ok(Some(ElementValue::Integer(3)))
            )),
        }

        // The storage recovers. The graph's retry of the retained input
        // reopens the fenced ledger from storage and resumes at the
        // completed prefix, in the same generation.
        boundary.outage.store(false, Ordering::Release);
        let recovered = observe_consumer(&control, &consumer, "recovered", |node| {
            node.failure.is_none() && node.lifecycle == ComponentLifecycle::Running
        })
        .await?;
        assert_eq!(recovered.generation, first.generation);
        // The source sends nothing more: the unacknowledged input is
        // delivered again from where the ledger stopped.
        tokio::time::timeout(BOUND, async {
            loop {
                let done = match mode {
                    sdk::ConsumerMode::External => effects(&path)?.0 == 3,
                    _ => {
                        count(inner.as_ref()).await.ok().flatten() == Some(ElementValue::Integer(3))
                    }
                };
                if done {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("retained input was not redelivered")??;
        let current = control.observed().components[&consumer].clone();
        assert!(current.failure.is_none(), "{current:?}");
        anyhow::Ok(())
    };
    // The run ends however the scenario ends.
    let scenario = async {
        let result = scenario.await;
        control.cancel();
        result
    };
    let (_, scenario) = tokio::join!(run, scenario);
    scenario?;
    graph.dispose().await?;
    if mode == sdk::ConsumerMode::External {
        // Each operation took effect once. Only the first one can have been
        // attempted more than once: retries during the outage repeat it while
        // its completion is unrecorded, and the destination deduplicates them.
        let (applied, attempts, distinct) = effects(&path)?;
        assert_eq!((applied, distinct), (3, 3));
        assert!(attempts >= if after { 3 } else { 4 }, "{attempts}");
    } else {
        assert_eq!(count(inner.as_ref()).await?, Some(ElementValue::Integer(3)));
    }
    Ok(())
}
