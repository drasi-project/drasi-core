// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::computation::{ComputationIndexProvider, InMemoryComputationProvider};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{computation::v1::*, DrasiLib};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::sync::{mpsc, Notify};

fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("component")
}
fn stream(name: &str) -> StreamId {
    StreamId::try_new(name).expect("stream")
}
fn endpoint(name: &str, port: &str) -> Endpoint {
    Endpoint::new(id(name), PortId::try_new(port).expect("port"))
}
fn owners(graph: &str) -> Vec<Arc<QuerySourceProgress>> {
    ["left", "right"]
        .into_iter()
        .map(|name| Arc::new(QuerySourceProgress::new(graph, id(name)).expect("owner")))
        .collect()
}
fn group(graph: &str, owners: &[Arc<QuerySourceProgress>]) -> Arc<ConsumerProgressSet> {
    Arc::new(ConsumerProgressSet::new(graph, id("source"), owners.iter().cloned()).expect("group"))
}

fn query_definition(owner: &QuerySourceProgress) -> ContinuousQueryDefinition {
    let alias = if owner.component_id().as_str() == "right" {
        "other"
    } else {
        "value"
    };
    ContinuousQueryDefinition {
        graph_id: owner.graph_id().into(),
        id: owner.component_id().clone(),
        query: format!("MATCH (n:items) RETURN n.id AS id, n.value AS {alias}"),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream(&format!("{}/results", owner.component_id())),
        outbox_capacity: NonZeroUsize::new(20).expect("capacity"),
    }
}

async fn query_for(
    owner: Arc<QuerySourceProgress>,
    provider: Arc<dyn ComputationIndexProvider>,
) -> Result<ContinuousQueryTransformer> {
    ContinuousQueryTransformer::new_configured(
        query_definition(&owner),
        provider,
        QueryOptions {
            recovery: QueryRecoveryPolicy::Strict,
            ..Default::default()
        },
        QueryExecutionSettings {
            source_transactions: Some(config(3306, MySqlOutput::Transactions).transactions),
            ..Default::default()
        },
        None,
    )
    .await?
    .with_source_progress(owner)
}

fn indexes(path: &Path) -> Result<Arc<dyn ComputationIndexProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    )))
}

struct Sink {
    descriptor: ComponentDescriptor,
    sender: mpsc::Sender<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for Sink {
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
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.sender.send(input.envelope).await?;
        Ok(())
    }
}

fn queries(
    mut builder: ComputationGraphBuilder,
    queries: Vec<ContinuousQueryTransformer>,
) -> Result<(ComputationGraphBuilder, Vec<mpsc::Receiver<ChangeEnvelope>>)> {
    let mut receivers = Vec::new();
    for query in queries {
        let name = query.descriptor().id().to_string();
        let sink = format!("{name}-sink");
        let (sender, receiver) = mpsc::channel(32);
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    id(&sink),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                sender,
            }))
            .bind_stream(endpoint(&name, "out"), stream(&format!("{name}/results")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(&name, "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(&name, "out"), endpoint(&sink, "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            );
        receivers.push(receiver);
    }
    Ok((builder, receivers))
}

fn resource(
    builder: ComputationGraphBuilder,
    name: &str,
    role: ResourceRole,
    handle: ResourceHandle,
) -> Result<ComputationGraphBuilder> {
    let id = ResourceId::try_new(name)?;
    Ok(builder
        .declare_resource(ResourceSpecification {
            id: id.clone(),
            role,
            ownership: ResourceOwnership::Borrowed,
            binding: Arc::from(name),
        })?
        .provide_resource(id, handle)?)
}

fn producer(
    builder: ComputationGraphBuilder,
    settings: &MySqlConfig,
    consumers: Arc<ConsumerProgressSet>,
    snapshots: &[Arc<MySqlSnapshot>],
    factory_created: bool,
) -> Result<ComputationGraphBuilder> {
    let builder = builder.bind_stream(endpoint("source", "out"), stream("changes"));
    if !factory_created {
        let source = if snapshots.is_empty() {
            MySqlSource::new_with_consumer_progress(
                id("source"),
                stream("changes"),
                settings.clone(),
                consumers,
            )?
        } else {
            MySqlSource::with_consumer_snapshots(
                id("source"),
                stream("changes"),
                settings.clone(),
                consumers,
                snapshots.iter().cloned(),
            )?
        };
        return Ok(builder.source(Box::new(source)));
    }
    let factory = Arc::new(MySqlSourceFactory::default());
    let mut specification = ComponentSpecification {
        descriptor: MySqlSource::describe(id("source"), MySqlOutput::Transactions)?,
        role: ComponentRole::Source,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: BTreeMap::from([
            (
                Arc::from("stream"),
                ConfigurationValue::Literal("changes".into()),
            ),
            (
                Arc::from("settings"),
                ConfigurationValue::Reference {
                    resource: ResourceId::try_new("settings")?,
                    key: Arc::from("mysql"),
                    secret: true,
                },
            ),
        ]),
        dependencies: BTreeMap::from([(
            Arc::from("consumer_progress"),
            vec![ResourceId::try_new("consumers")?],
        )]),
    };
    let mut builder = resource(
        builder,
        "consumers",
        ResourceRole::Checkpoint,
        ResourceHandle::new(ResourceRole::Checkpoint, consumers),
    )?;
    builder = resource(
        builder,
        "settings",
        ResourceRole::SecretStore,
        ResourceHandle::new(
            ResourceRole::SecretStore,
            Arc::new(ConfigurationResolverResource(Arc::new(Settings(
                serde_json::to_value(settings)?,
            )))),
        ),
    )?;
    for snapshot in snapshots {
        let name = format!("{}-bootstrap", snapshot.consumer_id());
        specification
            .dependencies
            .entry(Arc::from("consumer_snapshots"))
            .or_default()
            .push(ResourceId::try_new(name.as_str())?);
        builder = resource(
            builder,
            &name,
            ResourceRole::Bootstrap,
            ResourceHandle::new(ResourceRole::Bootstrap, snapshot.clone()),
        )?;
    }
    Ok(builder.component(specification, factory))
}

fn effects(envelope: &ChangeEnvelope, alias: &str) -> Result<Vec<(String, i64, String)>> {
    let mut effects = Vec::new();
    for operation in envelope.changes().operations() {
        let (kind, record) = match operation {
            ChangeOperation::Added { after, .. } => ("add", after),
            ChangeOperation::Updated { after, .. } => ("update", after),
            ChangeOperation::Deleted { before, .. } => {
                ("delete", before.as_ref().context("delete before image")?)
            }
        };
        let row =
            QueryChangeCodec::row_values_to_json(&QueryChangeCodec::decode_row(record)?.values);
        effects.push((
            kind.to_owned(),
            row["id"].as_i64().context("row id")?,
            row[alias].as_str().context("projected value")?.to_owned(),
        ));
    }
    effects.sort_by_key(|(_, id, _)| *id);
    Ok(effects)
}

async fn finish(core: &DrasiLib, database: Database, result: Result<()>) -> Result<()> {
    let shutdown = core.shutdown().await.map_err(anyhow::Error::from);
    let close = database.admin.close().await;
    let failures: Vec<_> = [result, shutdown, close]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if !failures.is_empty() {
        return Err(drasi_lib::error::OperationFailures::new(
            "MySQL shared graph and cleanup",
            failures,
        )
        .into());
    }
    Ok(())
}

#[test]
fn shared_construction_retains_real_owners_and_does_not_reinterpret_single_owner_snapshots(
) -> Result<()> {
    let owners = owners("graph");
    let consumers = group("graph", &owners);
    let mut settings = config(3306, MySqlOutput::Transactions);
    settings.replay = Some(MySqlRetention::UntilProcessed);
    settings.start = MySqlStartPosition::Position {
        file: "mysql-bin.000001".into(),
        position: 4,
    };
    let source = MySqlSource::new_with_consumer_progress(
        id("source"),
        stream("changes"),
        settings.clone(),
        consumers.clone(),
    )?;
    assert!(source.recovery_reader().is_none());
    assert!(source.recovery_progress().is_none());
    assert!(MySqlSource::new_with_consumer_progress(
        id("wrong"),
        stream("changes"),
        settings.clone(),
        consumers.clone(),
    )
    .is_err());
    let tables = BTreeMap::new();
    let single = source::source_binding(
        &settings,
        "server",
        &tables,
        &id("source"),
        &stream("changes"),
        Some(&SourceProgressReader::from(owners[0].clone())),
    )?;
    let shared = source::shared_source_binding(
        &settings,
        "server",
        &tables,
        &id("source"),
        &stream("changes"),
        "graph",
    )?;
    assert_ne!(single, shared);
    settings.start = MySqlStartPosition::Snapshot {
        lock_timeout_ms: NonZeroU64::new(2000).expect("timeout"),
    };
    let (_, snapshots) = MySqlSource::coordinated_consumers(
        id("source"),
        stream("changes"),
        settings.clone(),
        consumers.clone(),
    )?;
    assert_eq!(snapshots.len(), 2);
    assert!(MySqlSource::with_consumer_snapshots(
        id("source"),
        stream("changes"),
        settings.clone(),
        consumers.clone(),
        snapshots.iter().take(1).cloned(),
    )
    .is_err());
    assert!(MySqlSource::with_snapshot(
        id("source"),
        stream("changes"),
        settings.clone(),
        owners[0].clone(),
        snapshots[0].clone(),
    )
    .is_err());
    let impostor = Arc::new(QuerySourceProgress::new("graph", id("left"))?);
    let wrong = group("graph", &[impostor, owners[1].clone()]);
    assert!(MySqlSource::with_consumer_snapshots(
        id("source"),
        stream("changes"),
        settings.clone(),
        wrong,
        snapshots.iter().cloned(),
    )
    .is_err());
    let added = Arc::new(QuerySourceProgress::new("graph", id("third"))?);
    let replacement = group("graph", &[owners[0].clone(), owners[1].clone(), added]);
    let mut extended = snapshots;
    extended.push(Arc::new(MySqlSnapshot::for_consumer_group(
        id("source"),
        stream("changes"),
        settings.clone(),
        &replacement,
        &id("third"),
    )?));
    MySqlSource::with_consumer_snapshots(
        id("source"),
        stream("changes"),
        settings,
        replacement,
        extended,
    )?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_replay_rejects_volatile_actual_owners_before_opening_a_socket() -> Result<()> {
    let owners = owners("graph");
    let mut queries = Vec::new();
    for owner in &owners {
        let mut query = ContinuousQueryTransformer::new(
            query_definition(owner),
            Arc::new(InMemoryComputationProvider),
        )
        .await?
        .with_source_progress(owner.clone())?;
        query.start().await?;
        queries.push(query);
    }
    let mut settings = config(9, MySqlOutput::Transactions);
    settings.replay = Some(MySqlRetention::Server);
    settings.start = MySqlStartPosition::Position {
        file: "mysql-bin.000001".into(),
        position: 4,
    };
    let mut source = MySqlSource::new_with_consumer_progress(
        id("source"),
        stream("changes"),
        settings,
        group("graph", &owners),
    )?;
    let error = source.start().await.expect_err("persistent-only replay");
    assert!(error.to_string().contains("persistent"), "{error:#}");
    source.stop().await?;
    for query in &mut queries {
        query.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_query_rejects_a_same_named_bootstrap_reader_substitute_before_database_io() -> Result<()>
{
    let actual = owners("graph");
    let substitute = owners("graph");
    let mut settings = config(9, MySqlOutput::Transactions);
    settings.replay = Some(MySqlRetention::UntilProcessed);
    settings.start = MySqlStartPosition::Snapshot {
        lock_timeout_ms: NonZeroU64::new(1000).expect("timeout"),
    };
    let snapshot = Arc::new(MySqlSnapshot::for_consumer_group(
        id("source"),
        stream("changes"),
        settings,
        &group("graph", &substitute),
        &id("left"),
    )?);
    let path = tempfile::tempdir()?;
    let mut query = query_for(actual[0].clone(), indexes(path.path())?)
        .await?
        .with_bootstrap(snapshot);
    let started = query.start().await;
    let stopped = query.stop().await;
    let failure = started.expect_err("same name cannot substitute the actual owner");
    anyhow::ensure!(
        failure.to_string().contains("actual progress owner"),
        "{failure:#}"
    );
    stopped?;
    Ok(())
}

async fn graph_case(factory_created: bool, initialize: bool) -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    let path = tempfile::tempdir()?;
    let core = DrasiLib::builder()
        .with_id("mysql-shared-graph")
        .build()
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let graph_id = core
            .computation_control()?
            .desired_snapshot()
            .id
            .to_string();
        let owners = owners(&graph_id);
        let consumers = group(&graph_id, &owners);
        if initialize {
            database
                .admin
                .execute("INSERT INTO items VALUES (1,'initial'),(2,'second')")
                .await?;
            database.settings.start = MySqlStartPosition::Snapshot {
                lock_timeout_ms: NonZeroU64::new(2000).expect("timeout"),
            };
        }
        let provider = indexes(path.path())?;
        let snapshots = if initialize {
            let (_, snapshots) = MySqlSource::coordinated_consumers(
                id("source"),
                stream("changes"),
                database.settings.clone(),
                consumers.clone(),
            )?;
            snapshots
        } else {
            Vec::new()
        };
        let mut actual = Vec::new();
        let mut results = Vec::new();
        for owner in &owners {
            let mut query = query_for(owner.clone(), provider.clone()).await?;
            if let Some(snapshot) = snapshots
                .iter()
                .find(|snapshot| snapshot.consumer_id() == owner.component_id())
            {
                query = query.with_bootstrap(snapshot.clone());
            }
            results.push(query.results());
            actual.push(query);
        }
        let (builder, mut received) =
            queries(ComputationGraph::builder(graph_id.as_str()), actual)?;
        let builder = producer(
            builder,
            &database.settings,
            consumers,
            &snapshots,
            factory_created,
        )?;
        let report = core.add_components(builder.build_components()?).await?;
        anyhow::ensure!(report.committed, "{report:?}");
        core.start().await?;
        for name in ["source", "left", "right", "left-sink", "right-sink"] {
            core.computation_component(name)?.wait_started().await?;
        }
        for results in &results {
            anyhow::ensure!(
                results.snapshot()?.rows.len() == if initialize { 2 } else { 0 },
                "unexpected initial query rows"
            );
        }
        core.stop().await?;
        for (index, receiver) in received.iter_mut().enumerate() {
            while let Ok(initial) = receiver.try_recv() {
                let initial = effects(&initial, if index == 0 { "value" } else { "other" })?;
                anyhow::ensure!(
                    initialize
                        && initial.iter().all(|(kind, id, value)| kind == "add"
                            && ((*id == 1 && value == "initial")
                                || (*id == 2 && value == "second"))),
                    "unexpected initial output: {initial:?}"
                );
            }
        }
        core.start().await?;
        let heads: Vec<_> = results
            .iter()
            .map(|results| results.snapshot().map(|snapshot| snapshot.as_of_sequence))
            .collect::<std::result::Result<_, _>>()?;
        database.admin.execute("BEGIN").await?;
        if !initialize {
            database
                .admin
                .execute("INSERT INTO items VALUES (1,'initial'),(2,'second')")
                .await?;
        }
        database
            .admin
            .execute("UPDATE items SET value='final' WHERE id=1")
            .await?;
        database
            .admin
            .execute("DELETE FROM items WHERE id=2")
            .await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (3,'new')")
            .await?;
        database.admin.execute("COMMIT").await?;
        for (index, receiver) in received.iter_mut().enumerate() {
            let envelope = receiver.recv().await.context("query output closed")?;
            let actual = effects(&envelope, if index == 0 { "value" } else { "other" })?;
            let expected = if initialize {
                vec![
                    ("update".into(), 1, "final".into()),
                    ("delete".into(), 2, "second".into()),
                    ("add".into(), 3, "new".into()),
                ]
            } else {
                vec![
                    ("add".into(), 1, "final".into()),
                    ("add".into(), 3, "new".into()),
                ]
            };
            anyhow::ensure!(
                actual == expected,
                "live effects: expected {expected:?}, got {actual:?}"
            );
            anyhow::ensure!(
                results[index].snapshot()?.as_of_sequence == heads[index] + 1,
                "one transaction must produce one query output"
            );
            anyhow::ensure!(
                results[index].snapshot()?.rows.len() == 2,
                "live result rows"
            );
        }
        let previous: Vec<_> = owners
            .iter()
            .map(|owner| owner.snapshot().transport_sequences[&stream("changes")])
            .collect();
        core.stop().await?;
        database
            .admin
            .execute("UPDATE items SET value='offline' WHERE id=1")
            .await?;
        core.start().await?;
        for (index, receiver) in received.iter_mut().enumerate() {
            let envelope = receiver.recv().await.context("restart output closed")?;
            let actual = effects(&envelope, if index == 0 { "value" } else { "other" })?;
            anyhow::ensure!(
                actual == vec![("update".into(), 1, "offline".into())],
                "restart effects differ: {actual:?}"
            );
            anyhow::ensure!(
                results[index].snapshot()?.as_of_sequence == heads[index] + 2,
                "restart repeated a committed query effect"
            );
            anyhow::ensure!(
                owners[index].snapshot().transport_sequences[&stream("changes")] > previous[index],
                "source transport did not advance"
            );
        }
        core.stop().await?;
        for receiver in &mut received {
            anyhow::ensure!(
                receiver.try_recv().is_err(),
                "unexpected duplicate query output"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("shared MySQL graph deadline")
    .and_then(|result| result);
    finish(&core, database, result).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires container runtime"]
async fn one_source_two_actual_queries_direct_factory_snapshot_restart_current_thread() -> Result<()>
{
    for (factory, initialize) in [(false, false), (true, false), (false, true), (true, true)] {
        graph_case(factory, initialize).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires container runtime"]
async fn one_source_two_actual_queries_direct_factory_snapshot_restart_multi_thread() -> Result<()>
{
    for (factory, initialize) in [(false, false), (true, false), (false, true), (true, true)] {
        graph_case(factory, initialize).await?;
    }
    Ok(())
}

async fn replay_different_owners(add_member: bool) -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    let path = tempfile::tempdir()?;
    let core = DrasiLib::builder()
        .with_id("mysql-replay-group")
        .build()
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let graph_id = core
            .computation_control()?
            .desired_snapshot()
            .id
            .to_string();
        let mut owners = owners(&graph_id);
        let provider = indexes(path.path())?;
        let mut actual = Vec::new();
        for owner in &owners {
            actual.push(query_for(owner.clone(), provider.clone()).await?);
        }
        let mut seed = MySqlSource::new_with_consumer_progress(
            id("source"),
            stream("changes"),
            database.settings.clone(),
            group(&graph_id, &owners),
        )?;
        let seeded = async {
            for query in &mut actual {
                query.start().await?;
            }
            seed.start().await?;
            database
                .admin
                .execute("INSERT INTO items VALUES (1,'first')")
                .await?;
            let first = next(&mut seed).await?;
            for query in &mut actual {
                apply(query, first.clone()).await?;
            }
            database
                .admin
                .execute("UPDATE items SET value='second' WHERE id=1")
                .await?;
            apply(&mut actual[0], next(&mut seed).await?).await?;
            let key = SourceProgressKey::Source("source".into());
            anyhow::ensure!(
                owners[0].snapshot().checkpoints[&key].sequence
                    > owners[1].snapshot().checkpoints[&key].sequence,
                "fixture must use genuinely different committed binlog positions"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let mut failures = Vec::new();
        if let Err(error) = seeded {
            failures.push(error);
        }
        if let Err(error) = seed.stop().await {
            failures.push(error);
        }
        for query in &mut actual {
            if let Err(error) = query.stop().await {
                failures.push(error);
            }
        }
        if !failures.is_empty() {
            return Err(drasi_lib::error::OperationFailures::new(
                "MySQL replay preparation",
                failures,
            )
            .into());
        }
        if add_member {
            let owner = Arc::new(QuerySourceProgress::new(graph_id.as_str(), id("third"))?);
            actual.push(query_for(owner.clone(), provider.clone()).await?);
            owners.push(owner);
        }
        database.admin.execute("BEGIN").await?;
        database
            .admin
            .execute("DELETE FROM items WHERE id=1")
            .await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (2,'third')")
            .await?;
        database.admin.execute("COMMIT").await?;
        let results: Vec<_> = actual
            .iter()
            .map(ContinuousQueryTransformer::results)
            .collect();
        let (builder, mut received) =
            queries(ComputationGraph::builder(graph_id.as_str()), actual)?;
        let builder = producer(
            builder,
            &database.settings,
            group(&graph_id, &owners),
            &[],
            true,
        )?;
        let report = core.add_components(builder.build_components()?).await?;
        anyhow::ensure!(report.committed, "{report:?}");
        core.start().await?;
        for name in ["source", "left", "right"] {
            core.computation_component(name)?.wait_started().await?;
        }
        for (index, receiver) in received.iter_mut().enumerate() {
            let expected = match index {
                0 => vec![vec![
                    ("delete".into(), 1, "second".into()),
                    ("add".into(), 2, "third".into()),
                ]],
                1 => vec![
                    vec![("update".into(), 1, "second".into())],
                    vec![
                        ("delete".into(), 1, "second".into()),
                        ("add".into(), 2, "third".into()),
                    ],
                ],
                _ => vec![
                    vec![("add".into(), 1, "first".into())],
                    vec![("update".into(), 1, "second".into())],
                    vec![
                        ("delete".into(), 1, "second".into()),
                        ("add".into(), 2, "third".into()),
                    ],
                ],
            };
            for expected in expected {
                let envelope = receiver.recv().await.context("replayed output closed")?;
                let actual = effects(&envelope, if index == 1 { "other" } else { "value" })?;
                anyhow::ensure!(
                    actual == expected,
                    "replay expected {expected:?}, got {actual:?}"
                );
            }
        }
        core.stop().await?;
        for (index, result) in results.iter().enumerate() {
            let snapshot = result.snapshot()?;
            anyhow::ensure!(
                snapshot.rows.len() == 1 && snapshot.as_of_sequence == 3,
                "consumer {index} repeated or lost committed effects"
            );
            anyhow::ensure!(
                owners[index].snapshot().transport_sequences[&stream("changes")]
                    == if add_member { 5 } else { 4 },
                "replay transport must advance beyond every initial owner"
            );
        }
        for receiver in &mut received {
            anyhow::ensure!(
                receiver.try_recv().is_err(),
                "fast owner received a duplicate effect"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("MySQL unequal-owner replay deadline")
    .and_then(|result| result);
    finish(&core, database, result).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires container runtime"]
async fn unequal_committed_owners_and_fresh_members_replay_only_missing_effects_current_thread(
) -> Result<()> {
    for add_member in [false, true] {
        replay_different_owners(add_member).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires container runtime"]
async fn unequal_committed_owners_and_fresh_members_replay_only_missing_effects_multi_thread(
) -> Result<()> {
    for add_member in [false, true] {
        replay_different_owners(add_member).await?;
    }
    Ok(())
}

struct SnapshotGate {
    inner: Arc<MySqlSnapshot>,
    before: Option<Arc<Notify>>,
    captured: mpsc::Sender<ComponentId>,
    release: Arc<Notify>,
}

#[async_trait]
impl ComputationBootstrapProvider for SnapshotGate {
    fn validate_query_scope(
        &self,
        instance: &str,
        graph: &str,
        component: &ComponentId,
    ) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, component)
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.inner.recovery_reader()
    }

    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        use futures_util::StreamExt;
        if let Some(before) = &self.before {
            before.notified().await;
        }
        let snapshot = self.inner.snapshot_with_state(state).await?;
        self.captured.send(self.inner.consumer_id().clone()).await?;
        let release = self.release.clone();
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(
                futures_util::stream::once(async move {
                    release.notified().await;
                    snapshot.changes
                })
                .flatten(),
            ),
            watermarks: snapshot.watermarks,
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

struct SourceGate {
    inner: MySqlSource,
    ready: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for SourceGate {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        self.inner.recovery_contract()
    }
    async fn start(&mut self) -> Result<()> {
        self.inner.start().await?;
        self.ready.notify_one();
        self.release.notified().await;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }
}
#[async_trait]
impl EnvelopeSource for SourceGate {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.inner.next().await
    }
}

async fn unequal_snapshots() -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    let path = tempfile::tempdir()?;
    let core = DrasiLib::builder()
        .with_id("mysql-unequal-snapshots")
        .build()
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        database
            .admin
            .execute("INSERT INTO items VALUES (1,'initial'),(2,'second')")
            .await?;
        database.settings.start = MySqlStartPosition::Snapshot {
            lock_timeout_ms: NonZeroU64::new(2000).expect("timeout"),
        };
        let graph_id = core
            .computation_control()?
            .desired_snapshot()
            .id
            .to_string();
        let owners = owners(&graph_id);
        let (source, snapshots) = MySqlSource::coordinated_consumers(
            id("source"),
            stream("changes"),
            database.settings.clone(),
            group(&graph_id, &owners),
        )?;
        let provider = indexes(path.path())?;
        let second_begin = Arc::new(Notify::new());
        let snapshot_release = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        let source_ready = Arc::new(Notify::new());
        let source_release = Arc::new(Notify::new());
        let (captured, mut captures) = mpsc::channel(2);
        let mut actual = Vec::new();
        let mut results = Vec::new();
        for (index, owner) in owners.iter().enumerate() {
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot.consumer_id() == owner.component_id())
                .context("actual snapshot")?;
            let query = query_for(owner.clone(), provider.clone())
                .await?
                .with_bootstrap(Arc::new(SnapshotGate {
                    inner: snapshot.clone(),
                    before: (index == 1).then(|| second_begin.clone()),
                    captured: captured.clone(),
                    release: snapshot_release[index].clone(),
                }));
            results.push(query.results());
            actual.push(query);
        }
        let (builder, mut received) =
            queries(ComputationGraph::builder(graph_id.as_str()), actual)?;
        let builder = builder
            .source(Box::new(SourceGate {
                inner: source,
                ready: source_ready.clone(),
                release: source_release.clone(),
            }))
            .bind_stream(endpoint("source", "out"), stream("changes"));
        let report = core.add_components(builder.build_components()?).await?;
        anyhow::ensure!(report.committed, "{report:?}");
        let writes = async {
            anyhow::ensure!(captures.recv().await == Some(id("left")), "first snapshot");
            database.admin.execute("BEGIN").await?;
            database
                .admin
                .execute("UPDATE items SET value='middle' WHERE id=1")
                .await?;
            database
                .admin
                .execute("DELETE FROM items WHERE id=2")
                .await?;
            database
                .admin
                .execute("INSERT INTO items VALUES (3,'third')")
                .await?;
            database.admin.execute("COMMIT").await?;
            second_begin.notify_one();
            anyhow::ensure!(
                captures.recv().await == Some(id("right")),
                "second snapshot"
            );
            database.admin.execute("BEGIN").await?;
            database
                .admin
                .execute("UPDATE items SET value='final' WHERE id=1")
                .await?;
            database
                .admin
                .execute("DELETE FROM items WHERE id=3")
                .await?;
            database
                .admin
                .execute("INSERT INTO items VALUES (4,'fourth')")
                .await?;
            database.admin.execute("COMMIT").await?;
            for release in &snapshot_release {
                release.notify_one();
            }
            source_ready.notified().await;
            let key = SourceProgressKey::Source("source".into());
            anyhow::ensure!(
                owners[0].snapshot().checkpoints[&key].sequence
                    < owners[1].snapshot().checkpoints[&key].sequence,
                "queries must have different actual snapshot boundaries"
            );
            let heads = results
                .iter()
                .map(|results| results.snapshot().map(|snapshot| snapshot.as_of_sequence))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            source_release.notify_one();
            Ok::<_, anyhow::Error>(heads)
        };
        let (started, written) = tokio::join!(core.start(), writes);
        started?;
        let heads = written?;
        for (index, receiver) in received.iter_mut().enumerate() {
            let batches = if index == 0 {
                vec![
                    vec![
                        ("update".into(), 1, "middle".into()),
                        ("delete".into(), 2, "second".into()),
                        ("add".into(), 3, "third".into()),
                    ],
                    vec![
                        ("update".into(), 1, "final".into()),
                        ("delete".into(), 3, "third".into()),
                        ("add".into(), 4, "fourth".into()),
                    ],
                ]
            } else {
                vec![vec![
                    ("update".into(), 1, "final".into()),
                    ("delete".into(), 3, "third".into()),
                    ("add".into(), 4, "fourth".into()),
                ]]
            };
            for expected in batches {
                let actual = effects(
                    &receiver
                        .recv()
                        .await
                        .context("snapshot catch-up output closed")?,
                    if index == 0 { "value" } else { "other" },
                )?;
                anyhow::ensure!(
                    actual == expected,
                    "snapshot replay expected {expected:?}, got {actual:?}"
                );
            }
        }
        core.stop().await?;
        for (index, results) in results.iter().enumerate() {
            let result = results.snapshot()?;
            anyhow::ensure!(
                result.as_of_sequence == heads[index] + if index == 0 { 2 } else { 1 },
                "snapshot replay duplicated an already included transaction"
            );
            anyhow::ensure!(result.rows.len() == 2, "incomplete final snapshot");
        }
        for receiver in &mut received {
            anyhow::ensure!(
                receiver.try_recv().is_err(),
                "unexpected snapshot replay output"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("unequal MySQL snapshots deadline")
    .and_then(|result| result);
    finish(&core, database, result).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires container runtime"]
async fn independently_timed_snapshots_replay_concurrent_changes_without_stale_rows_current_thread(
) -> Result<()> {
    unequal_snapshots().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires container runtime"]
async fn independently_timed_snapshots_replay_concurrent_changes_without_stale_rows_multi_thread(
) -> Result<()> {
    unequal_snapshots().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires container runtime"]
async fn an_actual_stopped_member_fences_live_replication_until_awaited_cleanup() -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    let owners = owners("mysql-stopped-member");
    let path = tempfile::tempdir()?;
    let provider = indexes(path.path())?;
    let mut queries = Vec::new();
    for owner in &owners {
        queries.push(query_for(owner.clone(), provider.clone()).await?);
    }
    let mut source = MySqlSource::new_with_consumer_progress(
        id("source"),
        stream("changes"),
        database.settings.clone(),
        group("mysql-stopped-member", &owners),
    )?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        for query in &mut queries {
            query.start().await?;
        }
        source.start().await?;
        queries[1].stop().await?;
        let failure = next(&mut source)
            .await
            .expect_err("stopped owner must fence the source");
        anyhow::ensure!(failure.to_string().contains("ready"), "{failure:#}");
        anyhow::ensure!(
            source.start().await.is_err(),
            "failed source restarted without cleanup"
        );
        source.stop().await?;
        queries[1].start().await?;
        source.start().await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (1,'after cleanup')")
            .await?;
        let event = next(&mut source).await?;
        for query in &mut queries {
            apply(query, event.clone()).await?;
            anyhow::ensure!(
                query.results().snapshot()?.rows.len() == 1,
                "consumer did not recover"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("MySQL live member fencing deadline")
    .and_then(|result| result);
    let mut failures: Vec<_> = result.err().into_iter().collect();
    if let Err(error) = source.stop().await {
        failures.push(error);
    }
    for query in &mut queries {
        if let Err(error) = query.stop().await {
            failures.push(error);
        }
    }
    if let Err(error) = database.admin.close().await {
        failures.push(error);
    }
    if !failures.is_empty() {
        return Err(drasi_lib::error::OperationFailures::new(
            "MySQL live member fencing and cleanup",
            failures,
        )
        .into());
    }
    Ok(())
}
