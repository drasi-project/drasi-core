// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Standalone CW-81 contract, not the mixed SQLite/query/non-query CW-82 profile.
//! Build `cargo build -p drasi-host-sdk --example native_scheduling` first.

use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::PhysicalClock;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, ComputationTransaction,
        TransactionDomain,
    },
    interface::{
        FutureQueue, IndexError, IndexSet, OutboxPageLimits, SessionControl, StorageDurability,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_host_sdk::computation::{
    self, NativeComponentProxy, NativeFactory, NativePlugin, NativeProcessingResource,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use futures_util::FutureExt;
use serde_json::json;
use tokio::sync::{mpsc, Notify};

#[path = "fixtures/native_scheduling.rs"]
mod fixture;
#[path = "native_scheduling/restart.rs"]
mod restart;

const GRAPH: &str = "native-scheduling";
const COMPONENT: &str = "delay";
const TIMEOUT: Duration = Duration::from_secs(30);

struct Clock(AtomicU64);
impl Clock {
    fn new(now: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(now)))
    }
    fn advance_to(&self, now: u64) {
        self.0.store(now, Ordering::Release);
    }
}
impl PhysicalClock for Clock {
    fn now(&self) -> Result<u64> {
        Ok(self.0.load(Ordering::Acquire))
    }
}

fn library() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let debug = executable
        .parent()
        .and_then(Path::parent)
        .context("test target directory")?;
    let library = debug.join("examples").join(format!(
        "{}native_scheduling{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        library.is_file(),
        "missing scheduling fixture {}; build the native_scheduling example first",
        library.display()
    );
    Ok(library)
}

fn factory(loaded: bool) -> Result<Arc<NativeFactory>> {
    let plugin = if loaded {
        computation::load(library()?)?
    } else {
        NativePlugin::from_definition(fixture::plugin()?)?
    };
    plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == fixture::IMPLEMENTATION)
        .cloned()
        .context("missing real non-query scheduling factory")
}

fn provider(path: &Path) -> Result<Arc<dyn ComputationIndexProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    )))
}

fn create(
    loaded: bool,
    provider: Arc<dyn ComputationIndexProvider>,
    clock: Arc<Clock>,
    fail_input: bool,
    fail_due: bool,
) -> Result<NativeComponentProxy> {
    factory(loaded)?.create_component_with_processing_resource(
        ComponentId::try_new(COMPONENT)?,
        json!({"delay_ms":1000, "fail_input":fail_input, "fail_due":fail_due}),
        drasi_computation_plugin_sdk::Scope {
            instance_id: GRAPH.into(),
            graph_id: GRAPH.into(),
            generation: 1,
        },
        &NativeProcessingResource::new(provider).with_scheduling_clock(clock),
    )
}

fn input(key: &str, action: &str, value: u64, sequence: u64) -> Result<InputEnvelope> {
    let producer: GraphProducerIdentity = serde_json::from_value(json!({
        "construction_scope": GRAPH,
        "graph_id": GRAPH,
        "component_id": "source",
        "stream": "source/out",
        "incarnation": "9218f3c7-6d3b-4c54-9af7-10e4b46d36ae",
        "persistent": true,
    }))?;
    let mut envelope = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", key),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 123,
                },
                properties: ElementPropertyMap::from(json!({"value":value, "action":action})),
            },
        },
        producer.stream().clone(),
        sequence,
        Some(chrono::DateTime::from_timestamp_millis(123).context("logical event timestamp")?),
    )?;
    GraphProducerProgress::annotate(&mut envelope, &producer, sequence)?;
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope,
    })
}

fn assert_output(
    output: &OutputEnvelope,
    original: &ChangeEnvelope,
    due: u64,
    sequence: u64,
) -> Result<()> {
    assert_eq!(output.port, PortId::try_new("out")?);
    assert_eq!(
        output
            .envelope
            .system()
            .timestamp()
            .context("due timestamp")?
            .timestamp_millis(),
        due as i64
    );
    assert_eq!(
        GraphChangeCodec::decode_changes(&output.envelope)?,
        GraphChangeCodec::decode_changes(original)?
    );
    assert_eq!(
        output
            .envelope
            .lineage()
            .context("source provenance")?
            .envelope_id(),
        original.id()
    );
    assert_eq!(
        output
            .envelope
            .lineage()
            .context("source provenance")?
            .system()
            .timestamp(),
        original.system().timestamp()
    );
    let progress = GraphProducerProgress::from_envelope(&output.envelope)?
        .context("logical output identity")?;
    assert_eq!(progress.sequence(), sequence);
    assert_eq!(
        progress.identity().component_id(),
        &ComponentId::try_new(COMPONENT)?
    );
    assert!(progress.identity().persistent());
    Ok(())
}

struct Stored {
    completed: u64,
    next_due: Option<u64>,
    head: Option<u64>,
    outputs: Vec<(u64, Vec<u8>)>,
    element: Option<Arc<Element>>,
}

async fn stored(path: &Path) -> Result<Stored> {
    let owner =
        ComputationTransaction::try_new(provider(path)?.create_indexes(GRAPH, COMPONENT).await?)?;
    let resources = owner.resources();
    let checkpoints = resources.checkpoint_store().context("actual checkpoints")?;
    let completed = checkpoints
        .read_checkpoint(fixture::COMPLETED)
        .await?
        .map_or(0, |value| value.sequence);
    let state = Stored {
        completed,
        next_due: resources.indexes().future_queue.peek_due_time().await?,
        head: checkpoints.read_result_sequence(COMPONENT).await?,
        outputs: resources
            .outbox_writer()
            .context("actual outbox")?
            .read_page(
                COMPONENT,
                0,
                OutboxPageLimits {
                    max_records: NonZeroUsize::new(4).context("page count")?,
                    max_bytes: NonZeroUsize::new(1024 * 1024).context("page bytes")?,
                },
            )
            .await?,
        element: owner
            .run(async {
                Ok(resources
                    .indexes()
                    .element_index
                    .get_element(&ElementReference::new("source", "one"))
                    .await?)
            })
            .await?,
    };
    owner.shutdown().await?;
    Ok(state)
}

async fn lifecycle(loaded: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let clock = Clock::new(1000);
    let mut component = create(
        loaded,
        provider(directory.path())?,
        clock.clone(),
        false,
        false,
    )?;
    component.start().await?;
    assert!(component.continue_transform().await?.is_empty());
    let original = input("one", "request", 7, 1)?;
    assert!(component.transform(original.clone()).await?.is_empty());
    assert!(
        component.on_wakeup().await?.is_empty(),
        "physical deadline not reached"
    );
    let replacement = input("one", "replace", 9, 2)?;
    clock.advance_to(1500);
    component.transform(replacement.clone()).await?;
    clock.advance_to(2000);
    assert!(
        component.on_wakeup().await?.is_empty(),
        "old deadline was replaced"
    );
    clock.advance_to(2500);
    let output = component.on_wakeup().await?;
    assert_eq!(output.len(), 1);
    assert_output(&output[0], &replacement.envelope, 2500, 1)?;
    component.delivery_completed(&output).await?;
    component
        .transform(input("cancelled", "request", 10, 3)?)
        .await?;
    component
        .transform(input("cancelled", "cancel", 10, 4)?)
        .await?;
    clock.advance_to(10_000);
    assert!(
        component.on_wakeup().await?.is_empty(),
        "cancelled work must not emit"
    );
    assert!(
        !component
            .wakeup_source()
            .context("native wakeup")?
            .has_pending()
            .await?
    );
    component.stop().await?;
    drop(component);
    let state = stored(directory.path()).await?;
    assert_eq!(state.completed, 1);
    assert_eq!(state.next_due, None);
    assert_eq!(state.head, Some(1));
    assert_eq!(state.outputs.len(), 1);
    Ok(())
}

async fn failed_operations(loaded: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let clock = Clock::new(1000);
    let original = input("one", "request", 7, 1)?;
    let mut component = create(
        loaded,
        provider(directory.path())?,
        clock.clone(),
        true,
        false,
    )?;
    component.start().await?;
    component.continue_transform().await?;
    assert!(component.transform(original.clone()).await.is_err());
    component.stop().await?;
    drop(component);
    let state = stored(directory.path()).await?;
    assert!(state.element.is_none());
    assert_eq!(state.next_due, None);
    assert_eq!(state.completed, 0);

    let mut component = create(
        loaded,
        provider(directory.path())?,
        clock.clone(),
        false,
        true,
    )?;
    component.start().await?;
    component.continue_transform().await?;
    component.transform(original.clone()).await?;
    clock.advance_to(2000);
    assert!(component.on_wakeup().await.is_err());
    component.stop().await?;
    drop(component);
    let state = stored(directory.path()).await?;
    assert_eq!(state.next_due, Some(2000));
    assert_eq!(state.completed, 0);
    assert_eq!(state.head, Some(0));
    assert!(state.outputs.is_empty());

    let mut component = create(loaded, provider(directory.path())?, clock, false, false)?;
    component.start().await?;
    component.continue_transform().await?;
    let output = component.on_wakeup().await?;
    assert_eq!(output.len(), 1);
    assert_output(&output[0], &original.envelope, 2000, 1)?;
    component.delivery_completed(&output).await?;
    assert!(component.on_wakeup().await?.is_empty());
    component.stop().await?;
    drop(component);
    assert_eq!(stored(directory.path()).await?.completed, 1);
    Ok(())
}

#[derive(Default)]
struct CommitSignals {
    exit: AtomicU8,
    hold: AtomicBool,
    entered: Notify,
    release: Notify,
    scheduled: AtomicBool,
    changed: Notify,
}

struct ObservedProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    signals: Arc<CommitSignals>,
}

struct ObservedSession {
    inner: Arc<dyn SessionControl>,
    queue: Arc<dyn FutureQueue>,
    signals: Arc<CommitSignals>,
}

#[async_trait]
impl SessionControl for ObservedSession {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), IndexError> {
        let exit = self.signals.exit.swap(0, Ordering::AcqRel);
        if exit == 1 {
            std::process::exit(81);
        }
        if self.signals.hold.swap(false, Ordering::AcqRel) {
            self.signals.entered.notify_one();
            self.signals.release.notified().await;
        }
        self.inner.commit().await?;
        if exit == 2 {
            std::process::exit(82);
        }
        if self.queue.peek_due_time().await?.is_some() {
            self.signals.scheduled.store(true, Ordering::Release);
            self.signals.changed.notify_one();
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.inner.rollback()
    }
}

async fn commit_boundary(loaded: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let clock = Clock::new(1000);
    let signals = Arc::new(CommitSignals::default());
    let provider = Arc::new(ObservedProvider {
        inner: provider(directory.path())?,
        signals: signals.clone(),
    });
    let mut component = create(loaded, provider, clock.clone(), false, false)?;
    component.start().await?;
    component.continue_transform().await?;
    let wakeup = component.wakeup_source().context("scheduled wakeup")?;
    let original = input("one", "request", 7, 1)?;
    signals.hold.store(true, Ordering::Release);
    {
        let work = component.transform(original.clone());
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => anyhow::bail!("processing bypassed the commit gate: {result:?}"),
            _ = signals.entered.notified() => {},
        }
        clock.advance_to(2000);
        assert!(
            !wakeup.has_pending().await?,
            "uncommitted deadline must not be observed"
        );
        assert!(
            wakeup.wait().now_or_never().is_none(),
            "overdue staged work must not wake"
        );
        signals.release.notify_one();
        assert!(work.await?.is_empty());
    }
    assert!(wakeup.has_pending().await?);
    wakeup.wait().await?;
    let output = component.on_wakeup().await?;
    assert_eq!(output.len(), 1);
    assert_output(&output[0], &original.envelope, 2000, 1)?;
    component.delivery_completed(&output).await?;
    component.stop().await?;
    Ok(())
}

#[async_trait]
impl ComputationIndexProvider for ObservedProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, component).await?;
        let build = || -> Result<ComputationIndexes> {
            let indexes = original.indexes();
            let control: Arc<dyn SessionControl> = Arc::new(ObservedSession {
                inner: indexes.session_control.clone(),
                queue: indexes.future_queue.clone(),
                signals: self.signals.clone(),
            });
            let domain = TransactionDomain::new(control.clone());
            Ok(ComputationIndexes::try_new(
                IndexSet {
                    element_index: indexes.element_index.clone(),
                    archive_index: indexes.archive_index.clone(),
                    result_index: indexes.result_index.clone(),
                    future_queue: indexes.future_queue.clone(),
                    session_control: control,
                },
                Some(domain.clone()),
                Some(ComputationResource::participating(
                    original.checkpoint_store().context("checkpoints")?.clone(),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    original.outbox_writer().context("outbox")?.clone(),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    original.live_results_writer().context("rows")?.clone(),
                    &domain,
                )),
            )?
            .with_cleanup(original.cleanup().context("cleanup owner")?.clone())
            .with_durability(original.durability()))
        };
        build().map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))
    }
    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
    fn durability(&self) -> StorageDurability {
        self.inner.durability()
    }
}

struct Source {
    descriptor: ComponentDescriptor,
    envelope: Option<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for Source {
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
impl EnvelopeSource for Source {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.envelope.take().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("fixture output"),
            envelope,
        }))
    }
}
struct Capture {
    descriptor: ComponentDescriptor,
    outputs: mpsc::UnboundedSender<OutputEnvelope>,
}
#[async_trait]
impl ComputationComponent for Capture {
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
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.outputs.send(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: input.envelope,
        })?;
        Ok(())
    }
}

fn port(name: &str, direction: PortDirection) -> Result<PortDescriptor> {
    Ok(PortDescriptor::new(
        PortId::try_new(name)?,
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    ))
}

async fn automatic_graph(loaded: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let clock = Clock::new(1000);
    let signals = Arc::new(CommitSignals::default());
    let provider = Arc::new(ObservedProvider {
        inner: provider(directory.path())?,
        signals: signals.clone(),
    });
    let factory = factory(loaded)?;
    let component = ComponentId::try_new(COMPONENT)?;
    let source = ComponentId::try_new("source")?;
    let sink = ComponentId::try_new("sink")?;
    let resource = ResourceId::try_new("processing")?;
    let mut specification = factory.specification(component.clone(), json!({"delay_ms":1000}))?;
    specification
        .dependencies
        .insert("processing".into(), vec![resource.clone()]);
    let original = input("one", "request", 7, 1)?;
    let (outputs, mut received) = mpsc::unbounded_channel();
    let mut graph = ComputationGraph::builder(GRAPH)
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "processing".into(),
        })?
        .provide_resource(
            resource,
            ResourceHandle::new(
                ResourceRole::StateStore,
                Arc::new(
                    NativeProcessingResource::new(provider).with_scheduling_clock(clock.clone()),
                ),
            ),
        )?
        .source(Box::new(Source {
            descriptor: ComponentDescriptor::try_new(
                source.clone(),
                vec![port("out", PortDirection::Output)?],
            )?,
            envelope: Some(original.envelope.clone()),
        }))
        .component(specification, factory)
        .sink(Box::new(Capture {
            descriptor: ComponentDescriptor::try_new(
                sink.clone(),
                vec![port("in", PortDirection::Input)?],
            )?,
            outputs,
        }))
        .connect(
            EdgeDefinition::new(
                Endpoint::new(source.clone(), PortId::try_new("out")?),
                Endpoint::new(component.clone(), PortId::try_new("in")?),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(component.clone(), PortId::try_new("out")?),
                Endpoint::new(sink, PortId::try_new("in")?),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(
            Endpoint::new(source, PortId::try_new("out")?),
            StreamId::try_new("source/out")?,
        )
        .bind_stream(
            Endpoint::new(component, PortId::try_new("out")?),
            StreamId::try_new("delay/out")?,
        )
        .build()?;
    let run = graph.start()?;
    let mut phase = "input commit";
    let assertions = async {
        loop {
            let changed = signals.changed.notified();
            if signals.scheduled.load(Ordering::Acquire) {
                break;
            }
            changed.await;
        }
        assert!(
            received.try_recv().is_err(),
            "only a committed deadline can produce output"
        );
        phase = "due output";
        clock.advance_to(2000);
        let output = received
            .recv()
            .await
            .context("capture closed before scheduled output")?;
        assert_output(&output, &original.envelope, 2000, 1)?;
        Ok::<(), anyhow::Error>(())
    };
    let exercise = async {
        tokio::try_join!(async { run.await.map_err(anyhow::Error::from) }, assertions,)?;
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(10), exercise).await;
    let observed = graph.observed();
    let stopped = tokio::time::timeout(Duration::from_secs(10), graph.shutdown()).await;
    result.with_context(|| format!("scheduled graph failed during {phase}: {observed:?}"))??;
    stopped.context("scheduled graph cleanup timed out")??;
    assert!(
        received.try_recv().is_err(),
        "one request has one committed completion"
    );
    let state = stored(directory.path()).await?;
    assert_eq!(state.completed, 1);
    assert_eq!(state.next_due, None);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_transformer_current_thread() -> Result<()> {
    for loaded in [false, true] {
        tokio::time::timeout(TIMEOUT, async {
            commit_boundary(loaded).await?;
            lifecycle(loaded).await?;
            failed_operations(loaded).await
        })
        .await??;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_transformer_multi_thread() -> Result<()> {
    for loaded in [false, true] {
        tokio::time::timeout(TIMEOUT, async {
            commit_boundary(loaded).await?;
            lifecycle(loaded).await?;
            failed_operations(loaded).await
        })
        .await??;
    }
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
async fn scheduled_output_without_input_current_thread() -> Result<()> {
    for loaded in [false, true] {
        tokio::time::timeout(TIMEOUT, automatic_graph(loaded)).await??;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_output_without_input_multi_thread() -> Result<()> {
    for loaded in [false, true] {
        tokio::time::timeout(TIMEOUT, automatic_graph(loaded)).await??;
    }
    Ok(())
}
