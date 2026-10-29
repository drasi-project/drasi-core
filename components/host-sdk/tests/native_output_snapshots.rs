// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Generic output snapshots, end to end: a real native SSE sink reads source,
//! transformer and query-shaped outputs through the host's output-snapshot
//! capability, ABI and the graph's output catalogue. Every output is backed by
//! an independently written `SnapshotProvider` fixture; nothing pretends that a
//! live-only source has a snapshot.

#[path = "fixtures/native_sink_inputs.rs"]
mod sink_inputs;

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_host_sdk::computation::{NativeOutputSnapshotsResource, NativePlugin, OutputSelector};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;

const DEADLINE: Duration = Duration::from_secs(10);

fn stream(name: &str) -> StreamId {
    StreamId::try_new(name).expect("stream")
}

fn component_id(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("component")
}

fn port_id(name: &str) -> PortId {
    PortId::try_new(name).expect("port")
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// One output port's own state: rows, position, readiness and a gate.
struct Output {
    producer: GraphProducerIdentity,
    rows: Mutex<Vec<(String, i64)>>,
    sequence: AtomicU64,
    ready: AtomicBool,
    blocked: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl Output {
    fn new(component: &str, port: &str, rows: &[(&str, i64)]) -> Arc<Self> {
        Arc::new(Self {
            producer: GraphProducerIdentity::new(
                "snapshot-tests".into(),
                "snapshot-tests".into(),
                component_id(component),
                stream(&format!("{component}/{port}")),
                false,
            )
            .expect("producer identity"),
            rows: Mutex::new(
                rows.iter()
                    .map(|(key, count)| ((*key).to_owned(), *count))
                    .collect(),
            ),
            sequence: AtomicU64::new(rows.len() as u64),
            ready: AtomicBool::new(true),
            blocked: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        })
    }

    fn set(&self, key: &str, count: i64) {
        let mut rows = self.rows.lock().expect("fixture rows");
        rows.retain(|(existing, _)| existing != key);
        rows.push((key.to_owned(), count));
        self.sequence.fetch_add(1, Ordering::AcqRel);
    }

    fn remove(&self, key: &str) {
        self.rows
            .lock()
            .expect("fixture rows")
            .retain(|(existing, _)| existing != key);
        self.sequence.fetch_add(1, Ordering::AcqRel);
    }
}

fn record(key: &str, count: i64) -> Result<Record> {
    let mut properties = ElementPropertyMap::new();
    properties.insert("count", ElementValue::Integer(count));
    properties.insert("note", ElementValue::Null);
    let envelope = GraphChangeCodec::encode_changes(
        &[SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("inventory", key),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1,
                },
                properties,
            },
        }],
        stream("fixture/out"),
        1,
        None,
    )?;
    match envelope.changes().operations().first() {
        Some(ChangeOperation::Added { after, .. }) => Ok(after.clone()),
        other => anyhow::bail!("unexpected fixture operation {other:?}"),
    }
}

struct Provider(Arc<Output>);

#[async_trait]
impl SnapshotProvider for Provider {
    async fn snapshot(&self) -> std::result::Result<OutputSnapshot, OutputReadError> {
        if self.0.blocked.load(Ordering::Acquire) {
            self.0.entered.notify_one();
            self.0.release.notified().await;
        }
        if !self.0.ready.load(Ordering::Acquire) {
            return Err(OutputReadError::NotReady("fixture is not ready".into()));
        }
        let rows = self.0.rows.lock().expect("fixture rows").clone();
        let records = rows
            .iter()
            .map(|(key, count)| {
                let record = record(key, *count).map_err(OutputReadError::Failed)?;
                Ok((record.identity().clone(), record))
            })
            .collect::<std::result::Result<Vec<_>, OutputReadError>>()?;
        OutputSnapshot::try_new(
            OutputCursor::try_new(
                self.0.producer.clone(),
                self.0.sequence.load(Ordering::Acquire),
            )?,
            "inventory-dataset",
            GraphChangeCodec::schema().descriptor().clone(),
            records.into_iter().collect(),
        )
    }
}

fn port(id: &str, direction: PortDirection) -> PortDescriptor {
    PortDescriptor::new(
        port_id(id),
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}

fn readers(outputs: &[(&str, &Arc<Output>)]) -> OutputReaders {
    let mut readers = OutputReaders::new();
    for (name, output) in outputs {
        readers = readers.with_snapshot(port_id(name), Arc::new(Provider((*output).clone())));
    }
    readers
}

struct FixtureSource {
    descriptor: ComponentDescriptor,
    readers: OutputReaders,
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

async fn connect(
    core: &DrasiLib,
    from: (&ComponentHandle, &str),
    to: (&ComponentHandle, &str),
) -> Result<()> {
    let control = core.computation_control()?;
    let plan = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::Bind(DesiredRelationship {
                definition: EdgeDefinition {
                    from: Endpoint::new(from.0.id().clone(), port_id(from.1)),
                    to: Endpoint::new(to.0.id().clone(), port_id(to.1)),
                },
                policy: Default::default(),
                pipe: DesiredPipe::Bounded { capacity: 2 },
            })],
        )
        .await?;
    let report = control.reconcile(plan, TopologyBindings::default()).await?;
    anyhow::ensure!(report.failures.is_empty() && report.committed, "{report:?}");
    Ok(())
}

async fn add_sink(core: &DrasiLib, name: &str) -> Result<ComponentHandle> {
    Ok(core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::sink(Box::new(FixtureSink(
                ComponentDescriptor::try_new(
                    component_id(name),
                    vec![port("in", PortDirection::Input)],
                )?,
            ))))
            .auto_start(false),
        )
        .await?)
}

/// A source declaring the named output ports; each is connected to its own sink
/// so the component is constructed with every port bound.
async fn add_source(
    core: &DrasiLib,
    id: &str,
    outputs: &[(&str, Option<&Arc<Output>>)],
) -> Result<()> {
    let registered: Vec<(&str, &Arc<Output>)> = outputs
        .iter()
        .filter_map(|(name, output)| output.map(|output| (*name, output)))
        .collect();
    let mut addition =
        ComponentAddition::new(ConstructedComponent::source(Box::new(FixtureSource {
            descriptor: ComponentDescriptor::try_new(
                component_id(id),
                outputs
                    .iter()
                    .map(|(name, _)| port(name, PortDirection::Output))
                    .collect(),
            )?,
            readers: readers(&registered),
        })))
        .auto_start(false);
    for (name, _) in outputs {
        addition = addition.bind_stream(port_id(name), stream(&format!("{id}/{name}")));
    }
    let handle = core.add_computation_component(addition).await?;
    for (name, _) in outputs {
        let sink = add_sink(core, &format!("{id}-{name}-sink")).await?;
        connect(core, (&handle, name), (&sink, "in")).await?;
    }
    tokio::time::timeout(DEADLINE, handle.wait_created()).await??;
    Ok(())
}

async fn add_transformer(core: &DrasiLib, id: &str, output: &Arc<Output>) -> Result<()> {
    let handle = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::transformer(Box::new(
                FixtureTransformer {
                    descriptor: ComponentDescriptor::try_new(
                        component_id(id),
                        vec![
                            port("in", PortDirection::Input),
                            port("out", PortDirection::Output),
                        ],
                    )?,
                    readers: readers(&[("out", output)]),
                },
            )))
            .auto_start(false)
            .bind_stream(port_id("out"), stream(&format!("{id}/out"))),
        )
        .await?;
    let feed_id = format!("{id}-feed");
    add_source(core, &feed_id, &[("out", None)]).await?;
    let feed = core.computation_component(&feed_id)?;
    connect(core, (&feed, "out"), (&handle, "in")).await?;
    let sink = add_sink(core, &format!("{id}-sink")).await?;
    connect(core, (&handle, "out"), (&sink, "in")).await?;
    tokio::time::timeout(DEADLINE, handle.wait_created()).await??;
    Ok(())
}

fn binding(component: &str, port: &str) -> Value {
    json!({"component": component, "port": port})
}

async fn start_sink(
    core: &DrasiLib,
    sse_port: u16,
    max_readers: usize,
    allowed: &[(&str, Option<&str>)],
) -> Result<drasi_host_sdk::computation::NativeComponentProxy> {
    let plugin = NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?;
    let policy = Arc::new(NativeOutputSnapshotsResource::new(
        allowed
            .iter()
            .map(|(component, port)| {
                Ok(OutputSelector::new(
                    component_id(component),
                    port.map(PortId::try_new).transpose()?,
                ))
            })
            .collect::<Result<Vec<_>>>()?,
        Duration::from_secs(2),
        max_readers,
    )?);
    let catalogue = core.output_catalogue()?;
    let sink_configuration = json!({
        "outputs": {
            "main": binding("multi", "out"),
            "alt": binding("multi", "alt"),
            "primary": binding("xform", "out"),
            "bare": binding("bare", "out"),
            "denied": binding("denied", "out"),
        },
        "host": "127.0.0.1", "port": sse_port,
    });
    let mut sink = plugin.factories()[0].create_component_with_output_snapshots(
        component_id("ui"),
        sink_configuration.clone(),
        drasi_computation_plugin_sdk::Scope {
            instance_id: "snapshot-tests".into(),
            graph_id: catalogue.graph_id().to_string(),
            generation: 1,
        },
        policy,
        catalogue,
        sink_inputs::inputs_for(&sink_configuration),
    )?;
    sink.start().await?;
    Ok(sink)
}

struct World {
    core: DrasiLib,
    main: Arc<Output>,
    alt: Arc<Output>,
    xform: Arc<Output>,
}

async fn world(name: &str) -> Result<World> {
    let core = DrasiLib::builder().with_id(name).build().await?;
    core.start().await?;
    let main = Output::new("multi", "out", &[("a", 1), ("b", 2)]);
    let alt = Output::new("multi", "alt", &[("only", 9)]);
    let xform = Output::new("xform", "out", &[("x", 10), ("y", 20), ("z", 30)]);
    add_source(&core, "multi", &[("out", Some(&main)), ("alt", Some(&alt))]).await?;
    add_transformer(&core, "xform", &xform).await?;
    // A live-only output registers no snapshot provider at all.
    add_source(&core, "bare", &[("out", None)]).await?;
    let other = Output::new("denied", "out", &[("secret", 1)]);
    add_source(&core, "denied", &[("out", Some(&other))]).await?;
    Ok(World {
        core,
        main,
        alt,
        xform,
    })
}

async fn get(client: &reqwest::Client, port: u16, alias: &str) -> Result<reqwest::Response> {
    Ok(tokio::time::timeout(
        DEADLINE,
        client
            .get(format!("http://127.0.0.1:{port}/snapshot/{alias}"))
            .send(),
    )
    .await??)
}

fn by_id(changes: &Value) -> Result<Vec<(String, Value)>> {
    let mut rows: Vec<_> = changes
        .as_array()
        .context("snapshot array")?
        .iter()
        .map(|change| {
            Ok((
                change["identity"]["id"]
                    .as_str()
                    .context("identity id")?
                    .to_owned(),
                change.clone(),
            ))
        })
        .collect::<Result<_>>()?;
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(rows)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_transformer_and_second_port_snapshots_use_one_generic_path() -> Result<()> {
    let world = world("generic-snapshots").await?;
    let result = async {
        let sse_port = free_port()?;
        let mut sink = start_sink(
            &world.core,
            sse_port,
            4,
            &[
                ("multi", Some("out")),
                ("multi", Some("alt")),
                // No port: the component's primary (its single) output.
                ("xform", None),
                ("bare", Some("out")),
            ],
        )
        .await?;
        let client = reqwest::Client::new();

        // Source, first port: canonical ADD changes with real identity.
        let response = get(&client, sse_port, "main").await?;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "application/json");
        let epoch = response.headers()["x-drasi-output-epoch"]
            .to_str()?
            .to_owned();
        assert_eq!(response.headers()["x-drasi-output-position"], "2");
        assert!(response.headers().get("x-drasi-query-sequence").is_none());
        assert!(response.headers().get("x-drasi-query-generation").is_none());
        let body = response.json::<Value>().await?;
        let rows = by_id(&body)?;
        assert_eq!(rows.len(), 2);
        for (id, change) in &rows {
            assert_eq!(change["operation"], "ADD");
            assert_eq!(
                change["identity"],
                json!({"namespace": "inventory", "id": id})
            );
            assert_eq!(change["element"]["kind"], "node");
            assert_eq!(change["element"]["labels"], json!(["Item"]));
            assert!(change.get("before").is_none());
            for stale in ["row_signature", "rowSignature", "queryId", "metadata"] {
                assert!(change.get(stale).is_none(), "{stale}");
            }
        }
        assert_eq!(rows[0].0, "a");
        assert_eq!(rows[0].1["after"], json!({"count": 1, "note": null}));
        assert_eq!(rows[1].1["after"]["count"], 2);
        let handle = world
            .core
            .output_catalogue()?
            .resolve_component(&component_id("multi"), Some(&port_id("out")))?;
        assert_eq!(
            handle.snapshot().await?.cursor().epoch().to_string(),
            epoch,
            "the header is the producer's epoch, not a query generation"
        );

        // The second port of the same component is a distinct output.
        let alt = get(&client, sse_port, "alt").await?;
        assert_eq!(alt.status(), 200);
        assert_eq!(alt.headers()["x-drasi-output-position"], "1");
        let alt_epoch = alt.headers()["x-drasi-output-epoch"].to_str()?.to_owned();
        let alt_rows = by_id(&alt.json::<Value>().await?)?;
        assert_eq!(alt_rows.len(), 1);
        assert_eq!(alt_rows[0].0, "only");

        // A transformer, selected without a port, resolves through its primary.
        let primary = get(&client, sse_port, "primary").await?;
        assert_eq!(primary.status(), 200);
        assert_eq!(primary.headers()["x-drasi-output-position"], "3");
        let transformed = by_id(&primary.json::<Value>().await?)?;
        assert_eq!(
            transformed
                .iter()
                .map(|row| row.0.as_str())
                .collect::<Vec<_>>(),
            ["x", "y", "z"]
        );
        assert_ne!(epoch, alt_epoch, "each output port has its own epoch");

        // State changes are visible at the next capture.
        world.main.set("a", 100);
        world.main.remove("b");
        world.main.set("c", 3);
        let after = get(&client, sse_port, "main").await?;
        assert_eq!(after.headers()["x-drasi-output-position"], "5");
        let after = by_id(&after.json::<Value>().await?)?;
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].1["after"]["count"], 100);
        assert_eq!(after[1].0, "c");
        world.alt.set("only", 10);
        world.xform.remove("x");
        let shrunk = by_id(
            &get(&client, sse_port, "primary")
                .await?
                .json::<Value>()
                .await?,
        )?;
        assert_eq!(shrunk.len(), 2);

        // Capability absence, scope and unknown aliases are explicit.
        assert_eq!(get(&client, sse_port, "bare").await?.status(), 501);
        assert_eq!(get(&client, sse_port, "denied").await?.status(), 404);
        assert_eq!(get(&client, sse_port, "nobody").await?.status(), 404);
        world.main.ready.store(false, Ordering::Release);
        assert_eq!(get(&client, sse_port, "main").await?.status(), 503);
        world.main.ready.store(true, Ordering::Release);
        assert_eq!(get(&client, sse_port, "main").await?.status(), 200);

        sink.stop().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = world.core.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_readers_are_bounded_and_a_sink_without_the_capability_answers_501() -> Result<()>
{
    let world = world("bounded-snapshots").await?;
    let result = async {
        let sse_port = free_port()?;
        let mut sink = start_sink(&world.core, sse_port, 1, &[("multi", Some("out"))]).await?;
        let client = reqwest::Client::new();
        world.main.blocked.store(true, Ordering::Release);
        let first = {
            let client = client.clone();
            tokio::spawn(async move { get(&client, sse_port, "main").await })
        };
        tokio::time::timeout(DEADLINE, world.main.entered.notified()).await?;
        // The single reader is busy: a second request is refused, not queued.
        assert_eq!(get(&client, sse_port, "main").await?.status(), 429);
        world.main.blocked.store(false, Ordering::Release);
        world.main.release.notify_one();
        assert_eq!(first.await??.status(), 200);
        assert_eq!(get(&client, sse_port, "main").await?.status(), 200);
        sink.stop().await?;

        // Without the capability bound there is no snapshot, and no empty one.
        let bare_port = free_port()?;
        let plugin = NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?;
        let mut unbound = sink_inputs::create(
            &plugin.factories()[0],
            component_id("unbound"),
            json!({"outputs": {"main": binding("multi", "out")}, "host": "127.0.0.1", "port": bare_port}),
        )?;
        unbound.start().await?;
        assert_eq!(get(&client, bare_port, "main").await?.status(), 501);
        unbound.stop().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = world.core.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}
