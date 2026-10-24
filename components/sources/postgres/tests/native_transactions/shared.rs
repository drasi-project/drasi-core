// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_core::computation::InMemoryComputationProvider;
use drasi_host_sdk::computation::NativeFactory;
use drasi_lib::{
    channels::{SourceEvent, SourceEventWrapper},
    DrasiLib,
};
use drasi_postgres_common::replication::rows::RowPosition;
use serde_json::{json, Value};
use std::sync::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[path = "shared_bootstrap.rs"]
mod bootstrap;
#[path = "shared_transactions.rs"]
mod transactions;

const INSTANCE: &str = "postgres-native-libraries";
type Events = Arc<Mutex<Vec<ChangeEnvelope>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SharedOutput {
    Rows,
    Transactions,
}

// Hold accepted inputs before a real query's input boundary, without substituting
// its progress owner. Cancellation retains the receive permit on the receiver.
struct GatedPipe {
    inner: Box<dyn Pipe>,
    gate: Arc<Semaphore>,
    inputs: Events,
    accepted: Events,
}
impl Pipe for GatedPipe {
    fn capabilities(&self) -> &PipeCapabilities {
        self.inner.capabilities()
    }
    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        Arc::new(RecordedSender {
            inner: self.inner.sender(),
            accepted: self.accepted.clone(),
        })
    }

    fn take_receiver(&mut self) -> std::result::Result<Box<dyn EnvelopeReceiver>, PipeError> {
        Ok(Box::new(GatedReceiver {
            inner: self.inner.take_receiver()?,
            gate: self.gate.clone(),
            inputs: self.inputs.clone(),
            permit: None,
        }))
    }
}
struct RecordedSender {
    inner: Arc<dyn EnvelopeSender>,
    accepted: Events,
}

#[async_trait]
impl EnvelopeSender for RecordedSender {
    async fn send(
        &self,
        envelope: ChangeEnvelope,
    ) -> std::result::Result<EnqueueReceipt, SendFailure> {
        let recorded = envelope.clone();
        let receipt = self.inner.send(envelope).await?;
        self.accepted
            .lock()
            .expect("accepted input log")
            .push(recorded);
        Ok(receipt)
    }
}

struct GatedReceiver {
    inner: Box<dyn EnvelopeReceiver>,
    gate: Arc<Semaphore>,
    inputs: Events,
    permit: Option<OwnedSemaphorePermit>,
}
#[async_trait]
impl EnvelopeReceiver for GatedReceiver {
    async fn receive(&mut self) -> std::result::Result<Option<Delivery>, PipeError> {
        if self.permit.is_none() && !self.gate.is_closed() {
            self.permit = self.gate.clone().acquire_owned().await.ok();
        }
        let delivery = self.inner.receive().await?;
        if let Some(delivery) = &delivery {
            if let Some(permit) = self.permit.take() {
                permit.forget();
            }
            self.inputs
                .lock()
                .expect("input log")
                .push(delivery.envelope().clone());
        }
        Ok(delivery)
    }
}
struct GateControl {
    inner: Arc<dyn PipeControl>,
    gate: Arc<Semaphore>,
}
#[async_trait]
impl PipeControl for GateControl {
    fn close(&self) {
        self.gate.close();
        self.inner.close();
    }
    fn cancel(&self) {
        self.gate.close();
        self.inner.cancel();
    }
    async fn is_idle(&self) -> std::result::Result<bool, PipeError> {
        self.inner.is_idle().await
    }
}
struct GateProvider {
    gate: Arc<Semaphore>,
    inputs: Events,
    accepted: Events,
}
impl PipeProvider for GateProvider {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 32 }.capabilities()
    }
    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let provided = BoundedPipeConfig { capacity: 32 }.create()?;
        Ok(ProvidedPipe {
            pipe: Box::new(GatedPipe {
                inner: provided.pipe,
                gate: self.gate.clone(),
                inputs: self.inputs.clone(),
                accepted: self.accepted.clone(),
            }),
            control: Arc::new(GateControl {
                inner: provided.control,
                gate: self.gate.clone(),
            }),
        })
    }
}

struct Collect {
    descriptor: ComponentDescriptor,
    events: Events,
}
#[async_trait]
impl ComputationComponent for Collect {
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
impl EnvelopeSink for Collect {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.events.lock().expect("output log").push(input.envelope);
        Ok(())
    }
}

fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("test component ID")
}
fn stream() -> StreamId {
    StreamId::try_new("changes").expect("test stream")
}
fn factory(plugins: &libraries::Plugins) -> Result<Arc<NativeFactory>> {
    factory_for(plugins, SharedOutput::Rows)
}
fn factory_for(plugins: &libraries::Plugins, output: SharedOutput) -> Result<Arc<NativeFactory>> {
    let name = match output {
        SharedOutput::Rows => drasi_computation_source_postgres::POSTGRES_SHARED_ROWS,
        SharedOutput::Transactions => {
            drasi_computation_source_postgres::POSTGRES_SHARED_TRANSACTIONS
        }
    };
    plugins
        .source
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == name)
        .cloned()
        .context("shared PostgreSQL factory")
}
fn position(owner: &QuerySourceProgress) -> Result<Option<RowPosition>> {
    drasi_postgres_common::replication::rows::row_position(&owner.snapshot(), &id("source"))
}
async fn wait_until(label: &str, mut condition: impl FnMut() -> Result<bool>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition()? {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Result::<()>::Ok(())
    })
    .await
    .with_context(|| format!("timed out waiting for {label}"))?
}
async fn ordinary_query(
    name: &str,
    path: Option<&Path>,
    owner: Arc<QuerySourceProgress>,
) -> Result<ContinuousQueryTransformer> {
    processing_query(name, path, owner, SharedOutput::Rows).await
}

async fn processing_query(
    name: &str,
    path: Option<&Path>,
    owner: Arc<QuerySourceProgress>,
    output: SharedOutput,
) -> Result<ContinuousQueryTransformer> {
    processing_query_recovery(name, path, owner, output, QueryRecoveryPolicy::Strict).await
}

async fn processing_query_recovery(
    name: &str,
    path: Option<&Path>,
    owner: Arc<QuerySourceProgress>,
    output: SharedOutput,
    recovery: QueryRecoveryPolicy,
) -> Result<ContinuousQueryTransformer> {
    anyhow::ensure!(
        output != SharedOutput::Transactions || path.is_some(),
        "transaction query fixture requires persistent storage"
    );
    let indexes: Arc<dyn ComputationIndexProvider> = match path {
        Some(path) => libraries::indexes(&path.join(name))?,
        None => Arc::new(InMemoryComputationProvider),
    };
    let definition = ContinuousQueryDefinition {
        graph_id: ComponentBatch::GRAPH_ID.into(),
        id: id(name),
        query: "MATCH (n:person) RETURN n.id AS id,n.name AS name".into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new(format!("{name}-results"))?,
        outbox_capacity: size(32),
    };
    let query = ContinuousQueryTransformer::new_configured(
        definition,
        indexes,
        QueryOptions {
            recovery,
            ..Default::default()
        },
        QueryExecutionSettings {
            source_transactions: (output == SharedOutput::Transactions).then(|| limits(16)),
            ..Default::default()
        },
        None,
    )
    .await?;
    query
        .with_source_progress(owner)?
        .with_recovery_scope(INSTANCE)
}

struct SharedGraph {
    core: DrasiLib,
    owners: Vec<Arc<QuerySourceProgress>>,
    results: Vec<QueryResults>,
    inputs: Vec<Events>,
    outputs: Vec<Events>,
    gates: Vec<Arc<Semaphore>>,
    accepted: Vec<Events>,
}

async fn build_graph(
    plugins: &libraries::Plugins,
    database: &Database,
    path: Option<&Path>,
    slow_permits: usize,
    bootstrap: bool,
    snapshot_gates: Option<&[Arc<bootstrap::SnapshotGate>]>,
) -> Result<SharedGraph> {
    build_graph_mode(
        plugins,
        database,
        path,
        slow_permits,
        bootstrap,
        snapshot_gates,
        SharedOutput::Rows,
    )
    .await
}

async fn build_graph_mode(
    plugins: &libraries::Plugins,
    database: &Database,
    path: Option<&Path>,
    slow_permits: usize,
    bootstrap: bool,
    snapshot_gates: Option<&[Arc<bootstrap::SnapshotGate>]>,
    output: SharedOutput,
) -> Result<SharedGraph> {
    let owners = ["fast", "slow"]
        .into_iter()
        .map(|name| {
            Ok(Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id(name),
            )?))
        })
        .collect::<Result<Vec<_>>>()?;
    let progress = Arc::new(ConsumerProgressSet::new(
        ComponentBatch::GRAPH_ID,
        id("source"),
        owners.clone(),
    )?);
    let resource = ResourceId::try_new("consumers")?;
    let settings = ResourceId::try_new("settings")?;
    let factory = factory_for(plugins, output)?;
    let mut configuration = BTreeMap::from([
        (
            "stream".into(),
            ConfigurationValue::Literal(json!("changes")),
        ),
        (
            "settings".into(),
            ConfigurationValue::Reference {
                resource: settings.clone(),
                key: "postgres".into(),
                secret: true,
            },
        ),
    ]);
    if output == SharedOutput::Rows {
        for (name, value) in [
            ("persistent", json!(path.is_some())),
            ("auto_create_slot", json!(false)),
            ("start_from_slot", json!(false)),
            ("reconnect_attempts", json!(3)),
            ("reconnect_delay_ms", json!(100)),
        ] {
            configuration.insert(name.into(), ConfigurationValue::Literal(value));
        }
    }
    let specification = ComponentSpecification {
        descriptor: factory.metadata().descriptor(id("source"))?,
        role: ComponentRole::Source,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration,
        dependencies: BTreeMap::from([("consumer_progress".into(), vec![resource.clone()])]),
    };
    let mut builder = ComponentBatch::builder()
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::Checkpoint,
            ownership: ResourceOwnership::Borrowed,
            binding: "consumers".into(),
        })?
        .provide_resource(
            resource,
            ResourceHandle::new(ResourceRole::Checkpoint, progress),
        )?
        .declare_resource(ResourceSpecification {
            id: settings.clone(),
            role: ResourceRole::SecretStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "settings".into(),
        })?
        .provide_resource(
            settings,
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings(
                    serde_json::to_value(&database.config)?,
                )))),
            ),
        )?
        .component(specification, factory)
        .bind_stream(endpoint("source", "out"), stream());
    let mut result = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut accepted_logs = Vec::new();
    for (index, name) in ["fast", "slow"].into_iter().enumerate() {
        let mut query = processing_query(name, path, owners[index].clone(), output).await?;
        if bootstrap {
            let factory = plugins
                .bootstrap
                .bootstrap_factories()
                .iter()
                .find(|factory| {
                    factory.metadata().implementation.name.as_ref()
                        == match output {
                            SharedOutput::Rows => drasi_computation_bootstrap_postgres::POSTGRES_SHARED_ROW_BOOTSTRAP,
                            SharedOutput::Transactions => drasi_computation_bootstrap_postgres::POSTGRES_SHARED_TRANSACTION_BOOTSTRAP,
                        }
                })
                .context("shared PostgreSQL bootstrap factory")?;
            let provider = factory.create_provider(
                id(name),
                json!({"source":"source","stream":"changes","settings":database.config}),
                Some(libraries::scope(ComponentBatch::GRAPH_ID)),
                Some(owners[index].clone()),
            )?;
            query = query.with_bootstrap(match snapshot_gates {
                Some(gates) => bootstrap::gated(provider, gates[index].clone()),
                None => provider,
            });
        }
        result.0.push(query.results());
        let input: Events = Arc::default();
        let accepted: Events = Arc::default();
        let output: Events = Arc::default();
        let gate = Arc::new(Semaphore::new(if index == 0 { 128 } else { slow_permits }));
        let sink = format!("{name}-sink");
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Collect {
                descriptor: ComponentDescriptor::try_new(
                    id(&sink),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                events: output.clone(),
            }))
            .bind_stream(
                endpoint(name, "out"),
                StreamId::try_new(format!("{name}-results"))?,
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(GateProvider {
                    gate: gate.clone(),
                    inputs: input.clone(),
                    accepted: accepted.clone(),
                }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink, "in")),
                Box::new(BoundedPipeConfig { capacity: 32 }),
            );
        result.1.push(input);
        result.2.push(output);
        result.3.push(gate);
        accepted_logs.push(accepted);
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(builder.build()?)
        .build()
        .await?;
    Ok(SharedGraph {
        core,
        owners,
        results: result.0,
        inputs: result.1,
        outputs: result.2,
        gates: result.3,
        accepted: accepted_logs,
    })
}

async fn start_graph(graph: &SharedGraph) -> Result<()> {
    let started = async {
        graph.core.start().await?;
        for name in ["source", "fast", "slow", "fast-sink", "slow-sink"] {
            tokio::time::timeout(
                Duration::from_secs(10),
                graph.core.computation_component(name)?.wait_started(),
            )
            .await??;
        }
        Result::<()>::Ok(())
    }
    .await;
    if let Err(error) = started {
        graph
            .core
            .shutdown()
            .await
            .context("shared PostgreSQL graph cleanup")?;
        return Err(error);
    }
    Ok(())
}

async fn graph(
    plugins: &libraries::Plugins,
    database: &Database,
    path: Option<&Path>,
    slow_permits: usize,
) -> Result<SharedGraph> {
    let graph = build_graph(plugins, database, path, slow_permits, false, None).await?;
    start_graph(&graph).await?;
    Ok(graph)
}

async fn memory_snapshot_fanout() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'snapshot')")
            .await?;
        let graph = build_graph(&plugins, &database, None, 128, true, None).await?;
        start_graph(&graph).await?;
        for (index, owner) in graph.owners.iter().enumerate() {
            assert!(owner.snapshot().bootstrap_complete);
            assert!(!owner.snapshot().persistent);
            let position = position(owner)?.context("memory snapshot cursor")?;
            assert!(position.is_configured_snapshot());
            assert!(position.managed_slot.is_none());
            assert_eq!(
                values(&graph.results[index])?,
                [json!({"id":1,"name":"snapshot"})]
            );
        }
        database
            .client()
            .batch_execute("UPDATE person SET name='live'")
            .await?;
        wait_until("shared snapshot to live handover", || {
            Ok(graph
                .outputs
                .iter()
                .all(|events| events.lock().expect("output log").len() == 1))
        })
        .await?;
        for result in &graph.results {
            assert_eq!(values(result)?, [json!({"id":1,"name":"live"})]);
        }
        database
            .wait_feedback(
                position(&graph.owners[0])?
                    .context("live row position")?
                    .feedback_lsn(),
            )
            .await?;
        graph.core.shutdown().await?;
        drop(graph);
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_snapshot_memory_current_thread_static_and_dynamic() -> Result<()> {
    memory_snapshot_fanout().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_snapshot_memory_multi_thread_static_and_dynamic() -> Result<()> {
    memory_snapshot_fanout().await
}

fn values(results: &QueryResults) -> Result<Vec<Value>> {
    results
        .snapshot()?
        .rows
        .values()
        .map(|record| {
            let row = QueryChangeCodec::decode_row(record)?;
            Ok(QueryChangeCodec::row_values_to_json(&row.values))
        })
        .collect()
}

async fn memory_fanout() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        let graph = graph(&plugins, &database, None, 128).await?;
        database
            .client()
            .batch_execute(
                "BEGIN; INSERT INTO person VALUES(1,'first'); UPDATE person SET name='middle';
             UPDATE person SET name='last'; COMMIT;",
            )
            .await?;
        wait_until("two memory query results", || {
            Ok(graph
                .outputs
                .iter()
                .all(|events| events.lock().expect("output log").len() == 3))
        })
        .await?;
        for (index, owner) in graph.owners.iter().enumerate() {
            assert_eq!(
                values(&graph.results[index])?,
                [json!({"id":1,"name":"last"})]
            );
            assert!(!owner.snapshot().persistent);
            assert_eq!(owner.snapshot().transport_sequences[&stream()], 3);
            let position = position(owner)?.context("handled memory cursor")?;
            assert!(position.complete());
            assert_eq!(position.sequence, 3);
            database.wait_feedback(position.feedback_lsn()).await?;
        }
        graph.core.shutdown().await?;
        drop(graph);
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_memory_current_thread_static_and_dynamic() -> Result<()> {
    memory_fanout().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_memory_multi_thread_static_and_dynamic() -> Result<()> {
    memory_fanout().await
}

fn renumber(row: &OutputEnvelope, logical: u64, transport: u64) -> Result<OutputEnvelope> {
    let metadata = GraphChangeCodec::source_metadata(&row.envelope)?.context("row metadata")?;
    let mut position: RowPosition =
        serde_json::from_slice(&metadata.source_position.context("position")?)?;
    position.sequence = logical;
    let change = GraphChangeCodec::decode_changes(&row.envelope)?
        .into_iter()
        .next()
        .context("row change")?;
    let event = SourceEventWrapper::new(
        "source".into(),
        SourceEvent::Change(change),
        row.envelope.system().timestamp().context("timestamp")?,
        logical,
    )
    .with_source_position(serde_json::to_vec(&position)?.into());
    Ok(OutputEnvelope {
        port: row.port.clone(),
        envelope: GraphChangeCodec::encode_source_event(
            Arc::new(event),
            &id("source"),
            stream(),
            transport,
            None,
        )?,
    })
}

async fn seed_different_progress(
    plugins: &libraries::Plugins,
    database: &Database,
    path: &Path,
) -> Result<()> {
    let owners = ["fast", "slow"].map(|name| {
        Arc::new(QuerySourceProgress::new(ComponentBatch::GRAPH_ID, id(name)).expect("test owner"))
    });
    let mut fast = ordinary_query("fast", Some(path), owners[0].clone()).await?;
    let mut slow = ordinary_query("slow", Some(path), owners[1].clone()).await?;
    fast.start().await?;
    slow.start().await?;
    let consumers = Arc::new(ConsumerProgressSet::new(
        ComponentBatch::GRAPH_ID,
        id("source"),
        owners.clone(),
    )?);
    let mut source = factory(plugins)?.create_component_with_consumer_progress(
        id("source"),
        json!({
            "stream":"changes", "settings":database.config, "persistent":true,
            "auto_create_slot":false,"start_from_slot":false,
        }),
        libraries::scope(ComponentBatch::GRAPH_ID),
        consumers,
    )?;
    source.start().await?;
    database
        .client()
        .batch_execute(
            "BEGIN; INSERT INTO person VALUES(1,'one'); UPDATE person SET name='two';
         UPDATE person SET name='three'; COMMIT;",
        )
        .await?;
    let mut emitted = Vec::new();
    for _ in 0..3 {
        emitted.push(
            tokio::time::timeout(Duration::from_secs(5), source.next())
                .await??
                .context("seed row")?,
        );
    }
    source.stop().await?;
    // Real query transactions establish independent counters over actual WAL
    // rows. The source is stopped first: no fabricated feedback is published.
    apply(&mut fast, renumber(&emitted[0], 8_999, 19_999)?).await?;
    apply(&mut fast, renumber(&emitted[1], 9_000, 20_000)?).await?;
    apply(&mut slow, renumber(&emitted[0], 10_000, 2)?).await?;
    assert_eq!(
        position(&owners[0])?.context("fast seed cursor")?.offset,
        Some(1)
    );
    assert_eq!(
        position(&owners[1])?.context("slow seed cursor")?.offset,
        Some(0)
    );
    assert_eq!(database.feedback().await?, database.config.start_lsn);
    fast.stop().await?;
    slow.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_persistent_replay_uses_wal_minimum_and_maximum_of_all_counters() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        let path = tempfile::tempdir()?;
        seed_different_progress(&plugins, &database, path.path()).await?;
        let graph = graph(&plugins, &database, Some(path.path()), 128).await?;
        wait_until("different persistent owners to catch up", || {
            Ok(graph.owners.iter().all(|owner| {
                owner
                    .snapshot()
                    .transport_sequences
                    .get(&stream())
                    .is_some_and(|sequence| *sequence >= 20_002)
            }))
        })
        .await?;
        wait_until("only uncovered persistent effects", || {
            Ok(graph.outputs[0].lock().expect("fast output log").len() == 1
                && graph.outputs[1].lock().expect("slow output log").len() == 2)
        })
        .await?;
        for (index, owner) in graph.owners.iter().enumerate() {
            assert_eq!(
                values(&graph.results[index])?,
                [json!({"id":1,"name":"three"})]
            );
            assert!(owner.snapshot().persistent);
            let position = position(owner)?.context("persistent cursor")?;
            assert!(position.complete());
            assert_eq!(position.sequence, 20_002);
            let input = graph.inputs[index].lock().expect("input log");
            assert_eq!(input.len(), 2);
            assert_eq!(input[0].system().sequence(), 20_001);
            assert_eq!(input[1].system().sequence(), 20_002);
        }
        database
            .wait_feedback(
                position(&graph.owners[0])?
                    .context("fast cursor")?
                    .feedback_lsn(),
            )
            .await?;
        graph.core.shutdown().await?;
        drop(graph);
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_partial_feedback_and_reconnect_filter_queued_duplicates() -> Result<()> {
    for (dynamic, slow_permits) in [(false, 0), (true, 0), (false, 1), (true, 1)] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        let graph = graph(&plugins, &database, None, slow_permits).await?;
        let expected_inputs = 6 - slow_permits;
        database
            .client()
            .batch_execute(
                "BEGIN; INSERT INTO person VALUES(1,'one'); UPDATE person SET name='two';
             UPDATE person SET name='three'; COMMIT;",
            )
            .await?;
        wait_until("fast commit and slow partial transaction", || {
            Ok(
                position(&graph.owners[0])?.is_some_and(|position| position.complete())
                    && if slow_permits == 0 {
                        position(&graph.owners[1])?.is_none()
                    } else {
                        position(&graph.owners[1])?
                            .is_some_and(|position| position.offset == Some(0))
                    },
            )
        })
        .await?;
        if let Some(slow) = position(&graph.owners[1])? {
            assert!(!slow.complete());
            database.wait_feedback(slow.feedback_lsn()).await?;
        } else {
            database
                .wait_feedback(lsn(&database.config.start_lsn))
                .await?;
        }
        assert_eq!(database.feedback().await?, database.config.start_lsn);
        let old_pid: i32 = database
            .client()
            .query_one(
                "SELECT active_pid FROM pg_replication_slots WHERE slot_name='native_slot'",
                &[],
            )
            .await?
            .get(0);
        assert!(database
            .client()
            .query_one("SELECT pg_terminate_backend($1)", &[&old_pid])
            .await?
            .get::<_, bool>(0));
        wait_until("replayed rows after disconnection", || {
            Ok(graph.inputs[0].lock().expect("fast input log").len() >= expected_inputs)
        })
        .await?;
        assert_eq!(
            database.feedback().await?,
            database.config.start_lsn,
            "one complete owner cannot acknowledge the other owner's suffix"
        );
        assert_eq!(
            graph.outputs[0].lock().expect("fast output log").len(),
            3,
            "physically covered replay must not repeat fast-query effects"
        );
        graph.gates[1].add_permits(128);
        wait_until("queued and replayed slow rows", || {
            Ok(graph.owners.iter().all(|owner| {
                owner
                    .snapshot()
                    .transport_sequences
                    .get(&stream())
                    .is_some_and(|sequence| *sequence >= expected_inputs as u64)
            }))
        })
        .await?;
        wait_until("both query effect streams", || {
            Ok(graph
                .outputs
                .iter()
                .all(|output| output.lock().expect("output log").len() == 3))
        })
        .await?;
        for (index, owner) in graph.owners.iter().enumerate() {
            assert_eq!(
                values(&graph.results[index])?,
                [json!({"id":1,"name":"three"})]
            );
            assert_eq!(
                position(owner)?.context("handled cursor")?.sequence,
                3,
                "covered duplicates advance only transport"
            );
            let inputs = graph.inputs[index].lock().expect("input log");
            assert_eq!(inputs.len(), expected_inputs);
            for (index, input) in inputs.iter().enumerate() {
                assert_eq!(input.system().sequence(), index as u64 + 1);
            }
            let replayed: RowPosition = serde_json::from_slice(
                &GraphChangeCodec::source_metadata(&inputs[3])?
                    .context("replayed row metadata")?
                    .source_position
                    .context("replayed row cursor")?,
            )?;
            assert_eq!(replayed.offset, Some(slow_permits));
            assert_eq!(replayed.sequence, 4);
        }
        database
            .wait_feedback(
                position(&graph.owners[1])?
                    .context("slow cursor")?
                    .feedback_lsn(),
            )
            .await?;
        database
            .client()
            .batch_execute("UPDATE person SET name='after reconnect'")
            .await?;
        wait_until("post-reconnect transaction", || {
            Ok(graph
                .outputs
                .iter()
                .all(|output| output.lock().expect("output log").len() == 4))
        })
        .await?;
        for result in &graph.results {
            assert_eq!(values(result)?, [json!({"id":1,"name":"after reconnect"})]);
        }
        for owner in &graph.owners {
            assert_eq!(
                position(owner)?.context("post-reconnect cursor")?.sequence,
                expected_inputs as u64 + 1
            );
        }
        graph.core.shutdown().await?;
        drop(graph);
        database.shutdown().await?;
    }
    Ok(())
}
