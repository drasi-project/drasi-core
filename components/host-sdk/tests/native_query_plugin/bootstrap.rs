// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::ElementImage;
use drasi_host_sdk::computation::{
    NativeComponentProxy, NativeFactory, NativeOutputSnapshotsResource, OutputSelector,
};
use drasi_lib::DrasiLib;
use std::sync::atomic::{AtomicBool, Ordering};

#[path = "live_handover.rs"]
mod live_handover;
#[path = "provider.rs"]
pub(crate) mod provider;
#[path = "two_inputs.rs"]
mod two_inputs;

struct Snapshot {
    value: OutputSnapshot,
    unavailable: AtomicBool,
}

#[async_trait]
impl SnapshotProvider for Snapshot {
    async fn snapshot(&self) -> std::result::Result<OutputSnapshot, OutputReadError> {
        if self.unavailable.load(Ordering::Acquire) {
            return Err(OutputReadError::NotReady(
                "bootstrap fixture unavailable".into(),
            ));
        }
        Ok(self.value.clone())
    }
}

#[derive(Default)]
struct SnapshotGate {
    armed: AtomicBool,
    captured: Notify,
    release: Notify,
    snapshot: Mutex<Option<OutputSnapshot>>,
}

struct PausedSnapshot {
    inner: Arc<dyn SnapshotProvider>,
    gate: Arc<SnapshotGate>,
}

#[async_trait]
impl SnapshotProvider for PausedSnapshot {
    async fn snapshot(&self) -> std::result::Result<OutputSnapshot, OutputReadError> {
        let snapshot = self.inner.snapshot().await?;
        if self.gate.armed.swap(false, Ordering::AcqRel) {
            *self.gate.snapshot.lock().map_err(|_| {
                OutputReadError::Failed(anyhow::anyhow!("snapshot gate poisoned"))
            })? = Some(snapshot.clone());
            self.gate.captured.notify_one();
            self.gate.release.notified().await;
        }
        Ok(snapshot)
    }
}

struct ReleaseOnDrop(Arc<SnapshotGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release.notify_one();
    }
}

struct SnapshotSource {
    descriptor: ComponentDescriptor,
    snapshot: Arc<dyn SnapshotProvider>,
    events: tokio::sync::mpsc::Receiver<ChangeEnvelope>,
    emitted: Option<tokio::sync::watch::Sender<u64>>,
}

#[async_trait]
impl ComputationComponent for SnapshotSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new().with_snapshot(
            PortId::try_new("data").expect("constant port"),
            self.snapshot.clone(),
        )
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for SnapshotSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let Some(envelope) = self.events.recv().await else {
            return Ok(None);
        };
        if let Some(emitted) = &self.emitted {
            let progress = GraphProducerProgress::from_envelope(&envelope)?
                .context("fixture source event lacks producer progress")?;
            emitted.send_replace(progress.sequence());
        }
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("data").expect("constant port"),
            envelope,
        }))
    }
}

struct Fixture {
    core: DrasiLib,
    catalogue: OutputCatalogue,
    snapshot: Arc<Snapshot>,
    factory: Arc<NativeFactory>,
    binding: InputBinding,
    live: tokio::sync::mpsc::Sender<ChangeEnvelope>,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let plugin = computation::load(library())?;
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == "drasi.experimental/volatile-query"
            })
            .context("volatile query factory missing")?
            .clone();
        let source = ComponentId::try_new("source")?;
        let source_port = PortId::try_new("data")?;
        let stream = StreamId::try_new("source/data")?;
        let producer = GraphProducerIdentity::new(
            "bootstrap-test".into(),
            "bootstrap-test".into(),
            source.clone(),
            stream.clone(),
            false,
        )?;
        let records = [item_with_id("one", 1, 1), item_with_id("two", 2, 2)]
            .into_iter()
            .map(|element| {
                let record = GraphChangeCodec::element_record(&ElementImage::try_from(element)?)?;
                Ok((record.identity().clone(), record))
            })
            .collect::<Result<im::HashMap<_, _>>>()?;
        let snapshot = Arc::new(Snapshot {
            value: OutputSnapshot::try_new(
                OutputCursor::try_new(producer, 2)?,
                "items",
                GraphChangeCodec::schema().descriptor().clone(),
                records,
            )?,
            unavailable: AtomicBool::new(false),
        });
        let sink = ComponentId::try_new("unused-sink")?;
        let output = OutputEndpoint::new(source.clone(), source_port.clone());
        let binding = InputBinding {
            input_port: PortId::try_new("in")?,
            output,
            stream: stream.clone(),
        };
        let (live, events) = tokio::sync::mpsc::channel(4);
        let core = DrasiLib::builder()
            .with_id("bootstrap-test")
            .build()
            .await?;
        core.start().await?;
        let source_handle = core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::source(Box::new(SnapshotSource {
                    descriptor: ComponentDescriptor::try_new(
                        source.clone(),
                        vec![port("data", PortDirection::Output)?],
                    )?,
                    snapshot: snapshot.clone(),
                    events,
                    emitted: None,
                })))
                .auto_start(false)
                .bind_stream(source_port.clone(), stream),
            )
            .await?;
        core.add_computation_component(
            ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    sink.clone(),
                    vec![port("in", PortDirection::Input)?],
                )?,
                received: Arc::new(Mutex::new(Vec::new())),
                notification: Arc::new(Notify::new()),
            })))
            .auto_start(false),
        )
        .await?;
        connect(
            &core,
            vec![EdgeDefinition::new(
                Endpoint::new(source.clone(), source_port.clone()),
                Endpoint::new(sink, PortId::try_new("in")?),
            )],
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(10), source_handle.wait_created()).await??;
        let catalogue = core.output_catalogue()?;
        Ok(Self {
            core,
            catalogue,
            snapshot,
            factory,
            binding,
            live,
        })
    }

    fn configuration() -> serde_json::Value {
        json!({
            "query": "MATCH (n:Item) RETURN sum(n.value) AS total",
            "volatile": true,
            "bootstrap": [{"component": "source", "port": "data"}]
        })
    }

    fn query(&self) -> Result<NativeComponentProxy> {
        self.query_with("query", Self::configuration(), self.binding.clone())
    }

    fn query_with(
        &self,
        id: &str,
        configuration: serde_json::Value,
        binding: InputBinding,
    ) -> Result<NativeComponentProxy> {
        self.query_with_inputs(id, configuration, vec![binding])
    }

    fn query_with_inputs(
        &self,
        id: &str,
        configuration: serde_json::Value,
        bindings: Vec<InputBinding>,
    ) -> Result<NativeComponentProxy> {
        let policy = Arc::new(NativeOutputSnapshotsResource::new(
            bindings.iter().map(|binding| {
                OutputSelector::new(
                    binding.output.component.clone(),
                    Some(binding.output.port.clone()),
                )
            }),
            Duration::from_secs(2),
            1,
        )?);
        self.factory.create_component_with_output_snapshots(
            ComponentId::try_new(id)?,
            configuration,
            drasi_computation_plugin_sdk::Scope {
                instance_id: "bootstrap-test".into(),
                graph_id: self.catalogue.graph_id().to_string(),
                generation: 1,
            },
            policy,
            self.catalogue.clone(),
            bindings,
        )
    }

    fn input(&self, value: i64, sequence: u64) -> Result<InputEnvelope> {
        self.change(
            SourceChange::Replace {
                element: item(value, sequence + 10),
            },
            sequence,
        )
    }

    fn change(&self, change: SourceChange, sequence: u64) -> Result<InputEnvelope> {
        let producer = self.snapshot.value.cursor().producer();
        let mut envelope = GraphChangeCodec::encode_change(
            change,
            producer.stream().clone(),
            sequence + 100,
            None,
        )?;
        GraphProducerProgress::annotate(&mut envelope, producer, sequence)?;
        Ok(InputEnvelope {
            port: self.binding.input_port.clone(),
            envelope,
        })
    }
}

pub(crate) async fn connect(core: &DrasiLib, edges: Vec<EdgeDefinition>) -> Result<()> {
    connect_with_capacity(core, edges, 1).await
}

pub(crate) async fn connect_with_capacity(
    core: &DrasiLib,
    edges: Vec<EdgeDefinition>,
    capacity: usize,
) -> Result<()> {
    let control = core.computation_control()?;
    let plan = control
        .preview(
            control.desired_snapshot().revision,
            edges
                .into_iter()
                .map(|definition| {
                    DesiredMutation::Bind(DesiredRelationship {
                        definition,
                        policy: Default::default(),
                        pipe: DesiredPipe::Bounded { capacity },
                    })
                })
                .collect(),
        )
        .await?;
    let report = control.reconcile(plan, TopologyBindings::default()).await?;
    anyhow::ensure!(report.committed && report.failures.is_empty(), "{report:?}");
    Ok(())
}

fn snapshot_reader(query: &NativeComponentProxy) -> Result<Arc<dyn SnapshotProvider>> {
    query
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .cloned()
        .context("query snapshot provider missing")
}

fn assert_snapshot_total(snapshot: &OutputSnapshot, total: f64) -> Result<()> {
    assert_eq!(snapshot.records().len(), 1);
    let row = QueryChangeCodec::decode_row(
        snapshot
            .records()
            .values()
            .next()
            .context("snapshot result row missing")?,
    )?;
    assert_eq!(
        row.values.get("total"),
        Some(&VariableValue::from(json!(total)))
    );
    Ok(())
}

fn assert_total(output: &[OutputEnvelope], before: f64, after: f64) -> Result<()> {
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Aggregation {
            before: previous,
            after: current,
            ..
        }] => {
            assert_eq!(
                previous.as_ref().and_then(|row| row.get("total")),
                Some(&VariableValue::from(json!(before)))
            );
            assert_eq!(
                current.get("total"),
                Some(&VariableValue::from(json!(after)))
            );
        }
        _ => anyhow::bail!("unexpected bootstrapped aggregate result: {results:?}"),
    }
    Ok(())
}

async fn snapshot_then_live() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut query = fixture.query()?;
    query.start().await?;
    assert!(query.transform(fixture.input(99, 2)?).await?.is_empty());
    let output = query.transform(fixture.input(3, 3)?).await?;
    assert_total(&output, 3.0, 5.0)?;
    assert_eq!(output[0].envelope.system().sequence(), 1);
    assert!(query.transform(fixture.input(3, 3)?).await?.is_empty());
    assert_total(&query.transform(fixture.input(4, 4)?).await?, 5.0, 6.0)?;
    query.stop().await?;
    fixture.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_initializes_from_snapshot_then_processes_live_current_thread() -> Result<()> {
    snapshot_then_live().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_query_initializes_from_snapshot_then_processes_live_multi_thread() -> Result<()> {
    snapshot_then_live().await
}

#[tokio::test(flavor = "current_thread")]
async fn requested_bootstrap_requires_matching_input_and_host_service() -> Result<()> {
    let fixture = Fixture::new().await?;
    assert!(fixture
        .factory
        .create_component(ComponentId::try_new("query")?, Fixture::configuration())
        .is_err());
    let error = fixture
        .factory
        .create_component_with_inputs(
            ComponentId::try_new("query")?,
            Fixture::configuration(),
            vec![fixture.binding.clone()],
        )
        .err()
        .context("bootstrap without host service unexpectedly succeeded")?;
    assert!(
        format!("{error:#}").contains("output_snapshots"),
        "{error:#}"
    );
    let mut wrong_input = fixture.binding.clone();
    wrong_input.output.port = PortId::try_new("wrong")?;
    assert!(fixture
        .factory
        .create_component_with_inputs(
            ComponentId::try_new("query")?,
            Fixture::configuration(),
            vec![wrong_input],
        )
        .is_err());
    fixture.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_snapshot_does_not_start_query_and_can_retry_after_stop() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.snapshot.unavailable.store(true, Ordering::Release);
    let mut query = fixture.query()?;
    let reader = snapshot_reader(&query)?;
    assert!(reader.snapshot().await.is_err());
    let error = query.start().await.expect_err("snapshot must fail startup");
    assert!(
        format!("{error:#}").contains("bootstrap fixture unavailable"),
        "{error:#}"
    );
    assert!(query.transform(fixture.input(3, 3)?).await.is_err());
    assert!(reader.snapshot().await.is_err());
    query.stop().await?;
    fixture.snapshot.unavailable.store(false, Ordering::Release);
    query.start().await?;
    assert_snapshot_total(&snapshot_reader(&query)?.snapshot().await?, 3.0)?;
    assert_total(&query.transform(fixture.input(3, 3)?).await?, 3.0, 5.0)?;
    query.stop().await?;
    fixture.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_exposes_completed_initial_and_live_snapshots() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut query = fixture.query()?;
    query.start().await?;
    let initial = snapshot_reader(&query)?.snapshot().await?;
    assert_snapshot_total(&initial, 3.0)?;
    assert_eq!(initial.cursor().sequence(), 0);
    assert!(!initial.cursor().producer().persistent());
    let output = query.transform(fixture.input(3, 3)?).await?;
    let progress = GraphProducerProgress::from_envelope(&output[0].envelope)?
        .context("query live output has no producer progress")?;
    assert_eq!(progress.identity(), initial.cursor().producer());
    assert_eq!(progress.sequence(), 1);
    let current = snapshot_reader(&query)?.snapshot().await?;
    assert_snapshot_total(&current, 5.0)?;
    assert_eq!(current.cursor().sequence(), 1);
    assert_snapshot_total(&initial, 3.0)?;
    query.stop().await?;
    fixture.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_snapshot_removes_rows_and_fences_restart_epoch() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration = Fixture::configuration();
    configuration["query"] = json!("MATCH (n:Item) RETURN n.value AS value");
    let mut query = fixture.query_with("query", configuration, fixture.binding.clone())?;
    query.start().await?;
    let initial = snapshot_reader(&query)?.snapshot().await?;
    assert_eq!(initial.records().len(), 2);
    let output = query
        .transform(fixture.change(
            SourceChange::Delete {
                metadata: item(1, 13).get_metadata().clone(),
            },
            3,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let current = snapshot_reader(&query)?.snapshot().await?;
    assert_eq!(current.records().len(), 1);
    assert_eq!(current.cursor().sequence(), 1);
    let row = QueryChangeCodec::decode_row(current.records().values().next().unwrap())?;
    assert_eq!(
        row.values.get("value"),
        Some(&VariableValue::from(json!(2)))
    );
    let old_reader = snapshot_reader(&query)?;
    query.stop().await?;
    assert!(old_reader.snapshot().await.is_err());
    query.start().await?;
    let restarted = snapshot_reader(&query)?.snapshot().await?;
    assert_eq!(restarted.records().len(), 2);
    assert_eq!(restarted.cursor().sequence(), 0);
    assert_ne!(restarted.cursor().epoch(), initial.cursor().epoch());
    query.stop().await?;
    fixture.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_exposes_an_empty_initial_snapshot() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration = Fixture::configuration();
    configuration["query"] = json!("MATCH (n:Missing) RETURN n.value AS value");
    let mut query = fixture.query_with("query", configuration, fixture.binding.clone())?;
    query.start().await?;
    let snapshot = snapshot_reader(&query)?.snapshot().await?;
    assert!(snapshot.records().is_empty());
    assert_eq!(snapshot.cursor().sequence(), 0);
    let producer = snapshot.cursor().producer();
    assert_eq!(
        snapshot.dataset(),
        drasi_computation_contracts::output_identity::dataset_namespace(
            producer.construction_scope(),
            producer.graph_id(),
            producer.component_id(),
            &PortId::try_new("out")?,
        )?
    );
    query.stop().await?;
    fixture.core.shutdown().await?;
    Ok(())
}

async fn downstream_query_chain() -> Result<()> {
    let fixture = Fixture::new().await?;
    let upstream = fixture.query()?;
    let downstream = fixture.query_with(
        "downstream",
        json!({
            "query": "MATCH (n:Result) RETURN n.total AS total",
            "volatile": true,
            "bootstrap": [{"component": "query", "port": "out"}]
        }),
        InputBinding {
            input_port: PortId::try_new("in")?,
            output: OutputEndpoint::new(ComponentId::try_new("query")?, PortId::try_new("out")?),
            stream: StreamId::try_new("query/out")?,
        },
    )?;
    let upstream = fixture
        .core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::transformer(Box::new(upstream)))
                .auto_start(false)
                .bind_stream(PortId::try_new("out")?, StreamId::try_new("query/out")?),
        )
        .await?;
    let downstream = fixture
        .core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::transformer(Box::new(downstream)))
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out")?,
                    StreamId::try_new("downstream/out")?,
                ),
        )
        .await?;
    let received = Arc::new(Mutex::new(Vec::new()));
    let notification = Arc::new(Notify::new());
    let capture = fixture
        .core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("result-sink")?,
                    vec![port("in", PortDirection::Input)?],
                )?,
                received: received.clone(),
                notification: notification.clone(),
            })))
            .auto_start(false),
        )
        .await?;
    connect(
        &fixture.core,
        vec![
            EdgeDefinition::new(
                Endpoint::new(
                    fixture.binding.output.component.clone(),
                    fixture.binding.output.port.clone(),
                ),
                Endpoint::new(upstream.id().clone(), PortId::try_new("in")?),
            ),
            EdgeDefinition::new(
                Endpoint::new(upstream.id().clone(), PortId::try_new("out")?),
                Endpoint::new(downstream.id().clone(), PortId::try_new("in")?),
            ),
            EdgeDefinition::new(
                Endpoint::new(downstream.id().clone(), PortId::try_new("out")?),
                Endpoint::new(capture.id().clone(), PortId::try_new("in")?),
            ),
        ],
    )
    .await?;
    let run = async {
        capture.start().await?;
        fixture
            .core
            .computation_component("unused-sink")?
            .start()
            .await?;
        upstream.start().await?;
        downstream.start().await?;
        let endpoint = OutputEndpoint::new(downstream.id().clone(), PortId::try_new("out")?);
        let initial = fixture.catalogue.resolve(&endpoint)?.snapshot().await?;
        assert_snapshot_total(&initial, 3.0)?;
        assert_eq!(initial.cursor().sequence(), 0);
        assert!(received
            .lock()
            .map_err(|_| anyhow::anyhow!("query chain output poisoned"))?
            .is_empty());
        fixture
            .core
            .computation_component("source")?
            .start()
            .await?;
        fixture.live.send(fixture.input(99, 2)?.envelope).await?;
        for (sequence, value, total) in [(3, 3, 5.0), (4, 4, 6.0)] {
            fixture
                .live
                .send(fixture.input(value, sequence)?.envelope)
                .await?;
            notification.notified().await;
            let current = fixture.catalogue.resolve(&endpoint)?.snapshot().await?;
            assert_snapshot_total(&current, total)?;
            assert_eq!(current.cursor().sequence(), sequence - 2);
            assert_eq!(current.cursor().epoch(), initial.cursor().epoch());
        }
        let outputs = received
            .lock()
            .map_err(|_| anyhow::anyhow!("query chain output poisoned"))?;
        assert_eq!(outputs.len(), 2);
        for (output, (before, after)) in outputs.iter().zip([(3.0, 5.0), (5.0, 6.0)]) {
            match QueryChangeCodec::decode_evaluation(output)?.as_slice() {
                [QueryPartEvaluationContext::Updating {
                    before: previous,
                    after: current,
                    ..
                }] => {
                    assert_eq!(
                        previous.get("total"),
                        Some(&VariableValue::from(json!(before)))
                    );
                    assert_eq!(
                        current.get("total"),
                        Some(&VariableValue::from(json!(after)))
                    );
                }
                other => anyhow::bail!("unexpected downstream live result: {other:?}"),
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(10), run).await;
    fixture.core.shutdown().await?;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_loaded_query_bootstraps_and_runs_in_graph_current_thread() -> Result<()> {
    downstream_query_chain().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downstream_loaded_query_bootstraps_and_runs_in_graph_multi_thread() -> Result<()> {
    downstream_query_chain().await
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrapped_query_rejects_missing_or_changed_producer() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut query = fixture.query()?;
    query.start().await?;
    let unmarked = InputEnvelope {
        port: fixture.binding.input_port.clone(),
        envelope: GraphChangeCodec::encode_change(
            SourceChange::Replace {
                element: item(99, 99),
            },
            fixture.binding.stream.clone(),
            3,
            None,
        )?,
    };
    let error = query
        .transform(unmarked)
        .await
        .expect_err("missing producer progress must fail");
    assert!(format!("{error:#}").contains("requires producer progress"));
    let replacement = GraphProducerIdentity::new(
        "bootstrap-test".into(),
        "bootstrap-test".into(),
        fixture.binding.output.component.clone(),
        fixture.binding.stream.clone(),
        false,
    )?;
    let mut changed = fixture.input(99, 3)?;
    GraphProducerProgress::annotate(&mut changed.envelope, &replacement, 3)?;
    let error = query
        .transform(changed)
        .await
        .expect_err("changed producer epoch must fail");
    assert!(format!("{error:#}").contains("epoch changed"), "{error:#}");
    assert_total(&query.transform(fixture.input(3, 3)?).await?, 3.0, 5.0)?;
    query.stop().await?;
    fixture.core.shutdown().await?;
    Ok(())
}
