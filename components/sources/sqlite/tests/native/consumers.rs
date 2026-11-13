// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;

const FIRST_ID: &str = "00000000-0000-0000-0000-000000000601";
const SECOND_ID: &str = "00000000-0000-0000-0000-000000000602";
const THIRD_ID: &str = "00000000-0000-0000-0000-000000000603";
const VOLATILE_ID: &str = "00000000-0000-0000-0000-000000000604";

struct RetainedInitialSnapshot(ChangeEnvelope);
#[async_trait]
impl ComputationBootstrapProvider for RetainedInitialSnapshot {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let transaction = SourceTransactionCodec::decode(&self.0, limits())?;
        let changes = transaction
            .into_changes()
            .into_iter()
            .enumerate()
            .map(|(index, change)| {
                GraphChangeCodec::encode_change(change, stream("snapshot"), index as u64 + 1, None)
                    .map_err(Into::into)
            })
            .collect::<Vec<Result<ChangeEnvelope>>>();
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter(changes)),
            watermarks: vec![BootstrapWatermark {
                producer: None,
                stream: stream("changes"),
                source_id: Some(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                ),
                sequence: 1,
                position: self.0.system().source_position().cloned(),
            }],
        })
    }
}

fn owner(component_id: &str) -> Arc<QuerySourceProgress> {
    Arc::new(QuerySourceProgress::new("native-sqlite", id(component_id)).expect("consumer owner"))
}

fn group(owners: &[Arc<QuerySourceProgress>]) -> Arc<ConsumerProgressSet> {
    Arc::new(
        ConsumerProgressSet::new("native-sqlite", id(SOURCE_ID), owners.iter().cloned())
            .expect("consumer set"),
    )
}

async fn consumer(
    path: &Path,
    owner: Arc<QuerySourceProgress>,
    output_stream: &str,
) -> Result<ContinuousQueryTransformer> {
    consumer_with_recovery(path, owner, output_stream, QueryRecoveryPolicy::Strict).await
}

async fn consumer_with_recovery(
    path: &Path,
    owner: Arc<QuerySourceProgress>,
    output_stream: &str,
    recovery: QueryRecoveryPolicy,
) -> Result<ContinuousQueryTransformer> {
    consumer_with_text(
        path,
        owner,
        output_stream,
        recovery,
        "MATCH (n:items) RETURN n.id AS id, n.value AS value",
    )
    .await
}

async fn consumer_with_text(
    path: &Path,
    owner: Arc<QuerySourceProgress>,
    output_stream: &str,
    recovery: QueryRecoveryPolicy,
    text: &str,
) -> Result<ContinuousQueryTransformer> {
    let provider = Arc::new(RocksDbComputationProvider::new(
        path.join(owner.component_id().as_str()),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    ContinuousQueryTransformer::new_configured(
        ContinuousQueryDefinition {
            graph_id: "native-sqlite".into(),
            id: owner.component_id().clone(),
            query: text.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream(output_stream),
            outbox_capacity: size(8),
        },
        provider,
        QueryOptions {
            recovery,
            ..Default::default()
        },
        QueryExecutionSettings {
            source_transactions: Some(limits()),
            ..Default::default()
        },
        None,
    )
    .await?
    .with_source_progress(owner)
}

async fn group_replay() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = owner(FIRST_ID);
    let second = owner(SECOND_ID);
    let owners = group(&[first.clone(), second.clone()]);
    let mut source = SqliteSource::new_replayable_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        config(directory.path()),
        owners,
    )?;
    assert!(source.recovery_progress().is_none());
    let mut one = consumer(directory.path(), first, "first/out").await?;
    let mut two = consumer(directory.path(), second, "second/out").await?;
    one.start().await?;
    two.start().await?;
    source.start().await?;
    source.handle().execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'first')",
    ).await?;
    let original = next(&mut source).await?;
    apply(&mut one, original.clone()).await?;
    source.handle().query("SELECT id FROM items").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?.0, 1);
    assert_eq!(
        journal_state(directory.path())?.1,
        0,
        "slow consumer must retain the record"
    );
    source.start().await?;
    let replay = next(&mut source).await?;
    assert_eq!(
        replay.envelope.system().source_position(),
        original.envelope.system().source_position()
    );
    assert!(replay.envelope.system().sequence() > original.envelope.system().sequence());
    assert!(apply(&mut one, replay.clone()).await?.is_empty());
    apply(&mut two, replay).await?;
    assert_eq!(rows(&one), rows(&two));
    source.handle().query("SELECT id FROM items").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    one.stop().await?;
    two.stop().await?;
    drop(source);
    drop(one);
    drop(two);

    let first = owner(FIRST_ID);
    let second = owner(SECOND_ID);
    let mut one = consumer(directory.path(), first.clone(), "first/out").await?;
    let mut two = consumer(directory.path(), second.clone(), "second/out").await?;
    one.start().await?;
    two.start().await?;
    let mut source = SqliteSource::new_replayable_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        config(directory.path()),
        group(&[first, second]),
    )?;
    source.start().await?;
    assert!(timeout(Duration::from_millis(30), source.next())
        .await
        .is_err());
    source
        .handle()
        .execute("UPDATE items SET value='reopened'")
        .await?;
    let update = next(&mut source).await?;
    apply(&mut one, update.clone()).await?;
    apply(&mut two, update).await?;
    assert_eq!(rows(&one), [serde_json::json!({"id":1,"value":"reopened"})]);
    assert_eq!(rows(&one), rows(&two));
    source.stop().await?;
    one.stop().await?;
    two.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_journal_replays_for_every_consumer_current_thread() -> Result<()> {
    group_replay().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_journal_replays_for_every_consumer_multi_thread() -> Result<()> {
    group_replay().await
}

async fn coordinated_membership() -> Result<()> {
    let directory = tempfile::tempdir()?;
    rusqlite::Connection::open(directory.path().join("source.db"))?.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
         INSERT INTO items VALUES(1,'one'),(2,'two'),(3,'three'),(4,'four')",
    )?;
    let first = owner(FIRST_ID);
    let second = owner(SECOND_ID);
    let (mut source, snapshots) = SqliteSource::coordinated_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        config(directory.path()),
        group(&[first.clone(), second.clone()]),
    )?;
    let mut one = consumer(directory.path(), first.clone(), "first/out")
        .await?
        .with_bootstrap(snapshots.for_consumer(&id(FIRST_ID))?);
    let mut two = consumer(directory.path(), second.clone(), "second/out")
        .await?
        .with_bootstrap(snapshots.for_consumer(&id(SECOND_ID))?);
    assert!(source.handle().query("SELECT 1").await.is_err());
    one.start().await?;
    assert_eq!(rows(&one).len(), 4);
    snapshots.for_consumer(&id(FIRST_ID))?.stop().await?;
    assert!(source.handle().query("SELECT 1").await.is_err());
    two.start().await?;
    assert_eq!(rows(&one), rows(&two));
    for owner in [&first, &second] {
        let state = owner.snapshot();
        let position = &state.checkpoints[&SourceProgressKey::Source(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
        )];
        assert_eq!(position.sequence, 0);
        assert!(position.source_position.is_some() && state.persistent && state.bootstrap_complete);
    }
    source.start().await?;
    source
        .handle()
        .execute_batch(
            "UPDATE items SET value='changed' WHERE id=1; INSERT INTO items VALUES(5,'five')",
        )
        .await?;
    let transaction = next(&mut source).await?;
    apply(&mut one, transaction).await?;
    source.handle().query("SELECT id FROM items").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?.1, 0);
    one.stop().await?;
    two.stop().await?;
    drop(source);
    drop(one);
    drop(two);
    drop(snapshots);

    let first = owner(FIRST_ID);
    let second = owner(SECOND_ID);
    let third = owner(THIRD_ID);
    let (mut source, snapshots) = SqliteSource::coordinated_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        config(directory.path()),
        group(&[first.clone(), second.clone(), third.clone()]),
    )?;
    let mut one = consumer(directory.path(), first, "first/out")
        .await?
        .with_bootstrap(snapshots.for_consumer(&id(FIRST_ID))?);
    let mut two = consumer(directory.path(), second, "second/out")
        .await?
        .with_bootstrap(snapshots.for_consumer(&id(SECOND_ID))?);
    let mut three = consumer(directory.path(), third.clone(), "third/out")
        .await?
        .with_bootstrap(snapshots.for_consumer(&id(THIRD_ID))?);
    // A newly enrolled query may initialize before the older members restart.
    three.start().await?;
    assert_eq!(rows(&three).len(), 5);
    assert_eq!(
        third.snapshot().checkpoints[&SourceProgressKey::Source(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID")
        )]
            .sequence,
        1
    );
    one.start().await?;
    two.start().await?;
    assert_eq!(rows(&one).len(), 5);
    assert_eq!(rows(&two).len(), 4);
    source.start().await?;
    let replay = next(&mut source).await?;
    assert!(apply(&mut one, replay.clone()).await?.is_empty());
    apply(&mut two, replay.clone()).await?;
    assert!(apply(&mut three, replay).await?.is_empty());
    assert_eq!(rows(&one), rows(&two));
    assert_eq!(rows(&one), rows(&three));
    source.handle().query("SELECT id FROM items").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    one.stop().await?;
    two.stop().await?;
    three.stop().await?;
    drop(source);
    drop(one);
    drop(two);
    drop(three);
    drop(snapshots);

    let first = owner(FIRST_ID);
    let third = owner(THIRD_ID);
    let (mut source, snapshots) = SqliteSource::coordinated_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        config(directory.path()),
        group(&[first.clone(), third.clone()]),
    )?;
    let mut one = consumer(directory.path(), first, "first/out")
        .await?
        .with_bootstrap(snapshots.for_consumer(&id(FIRST_ID))?);
    let mut three = consumer(directory.path(), third, "third/out")
        .await?
        .with_bootstrap(snapshots.for_consumer(&id(THIRD_ID))?);
    one.start().await?;
    three.start().await?;
    source.start().await?;
    source
        .handle()
        .execute("DELETE FROM items WHERE id=5")
        .await?;
    let deletion = next(&mut source).await?;
    apply(&mut one, deletion.clone()).await?;
    apply(&mut three, deletion).await?;
    assert_eq!(rows(&one).len(), 4);
    assert_eq!(rows(&one), rows(&three));
    source.handle().query("SELECT id FROM items").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?, (2, 2, 0));
    one.stop().await?;
    three.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_snapshots_coordinate_initial_load_and_membership_current_thread() -> Result<()> {
    coordinated_membership().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_snapshots_coordinate_initial_load_and_membership_multi_thread() -> Result<()> {
    coordinated_membership().await
}

async fn factory_graph() -> Result<()> {
    for coordinated in [false, true] {
        let directory = tempfile::tempdir()?;
        let settings = config(directory.path());
        if coordinated {
            rusqlite::Connection::open(directory.path().join("source.db"))?.execute_batch(
                "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(0,'initial')",
            )?;
        }
        let first = owner(FIRST_ID);
        let second = owner(SECOND_ID);
        let consumers = group(&[first.clone(), second.clone()]);
        let snapshots = if coordinated {
            let (_, snapshots) = SqliteSource::coordinated_with_consumers(
                id(SOURCE_ID),
                stream("changes"),
                settings.clone(),
                consumers.clone(),
            )?;
            Some(snapshots)
        } else {
            None
        };
        let client = Arc::new(SqliteClientResource::new(id(SOURCE_ID), settings.clone())?);
        let handle = client.handle();
        let factory = Arc::new(SqliteSourceFactory::default());
        let mut builder = ComputationGraph::builder("native-sqlite");
        let mut results = Vec::new();
        let mut receivers = Vec::new();
        let endpoint =
            |name: &str, port: &str| Endpoint::new(id(name), PortId::try_new(port).expect("port"));
        let endpoint_id = |id: &ComponentId, port: &str| {
            Endpoint::new(id.clone(), PortId::try_new(port).expect("port"))
        };
        let sink_ids = [
            ComponentId::try_new("00000000-0000-0000-0000-000000000501")?,
            ComponentId::try_new("00000000-0000-0000-0000-000000000502")?,
        ];
        for ((name, output_stream, owner), sink_id) in [
            ("first", "first/out", &first),
            ("second", "second/out", &second),
        ]
        .into_iter()
        .zip(sink_ids)
        {
            let mut query = consumer(directory.path(), owner.clone(), output_stream).await?;
            if let Some(snapshots) = &snapshots {
                query = query.with_bootstrap(snapshots.for_consumer(owner.component_id())?);
            }
            results.push(query.results());
            let (sink, receiver) = drasi_reaction_application::NativeApplicationReaction::channel(
                sink_id.clone(),
                QueryChangeCodec::schema().descriptor().clone(),
                size(4),
            )?;
            receivers.push(receiver);
            builder = builder
                .transformer(
                    drasi_lib::computation::v1::ComponentName::try_new(format!("query-{name}"))
                        .expect("fixture component name"),
                    Box::new(query),
                )
                .sink(
                    drasi_lib::computation::v1::ComponentName::try_new(format!("sink-{name}"))
                        .expect("fixture component name"),
                    Box::new(sink),
                )
                .bind_stream(
                    endpoint_id(owner.component_id(), "out"),
                    stream(output_stream),
                )
                .connect(
                    EdgeDefinition::new(
                        endpoint(SOURCE_ID, "out"),
                        endpoint_id(owner.component_id(), "in"),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    EdgeDefinition::new(
                        endpoint_id(owner.component_id(), "out"),
                        endpoint_id(&sink_id, "in"),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
        }
        let mut dependencies = std::collections::BTreeMap::from([
            (Arc::from("client"), vec![ResourceId::try_new("client")?]),
            (
                Arc::from("consumer_progress"),
                vec![ResourceId::try_new("consumers")?],
            ),
        ]);
        let mut resources = vec![
            (
                "client",
                ResourceRole::Component,
                ResourceHandle::new(ResourceRole::Component, client),
            ),
            (
                "consumers",
                ResourceRole::Checkpoint,
                ResourceHandle::new(ResourceRole::Checkpoint, consumers.clone()),
            ),
        ];
        if let Some(snapshots) = snapshots {
            dependencies.insert(
                Arc::from("consumer_snapshots"),
                vec![ResourceId::try_new("snapshots")?],
            );
            resources.push((
                "snapshots",
                ResourceRole::Bootstrap,
                ResourceHandle::new(ResourceRole::Bootstrap, snapshots),
            ));
        }
        for (name, role, handle) in resources {
            let resource = ResourceId::try_new(name)?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: name.into(),
                })?
                .provide_resource(resource, handle)?;
        }
        let specification = ComponentSpecification {
            descriptor: SqliteSource::describe(id(SOURCE_ID), settings.output)?,
            role: ComponentRole::Source,
            completion: None,
            implementation: factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: std::collections::BTreeMap::from([
                (
                    Arc::from("stream"),
                    ConfigurationValue::Literal("changes".into()),
                ),
                (
                    Arc::from("settings"),
                    ConfigurationValue::Literal(serde_json::to_value(settings)?),
                ),
                (
                    Arc::from("coordinated_snapshot"),
                    ConfigurationValue::Literal(coordinated.into()),
                ),
            ]),
            dependencies,
        };
        factory.validate(&specification)?;
        let mut graph = builder
            .component(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-component-1")
                    .expect("fixture component name"),
                specification,
                factory,
            )
            .bind_stream(endpoint(SOURCE_ID, "out"), stream("changes"))
            .build()?;
        let run = graph.run()?;
        let control = run.control();
        let outcome = timeout(Duration::from_secs(20), async {
            tokio::join!(run, async {
                let outcome = async {
                    let startup = control.start_components(GraphRevision(1), GraphSelection::All).await?;
                    anyhow::ensure!(startup.summary == OperationSummary::Completed, "{startup:?}");
                    for result in &results { assert_eq!(result.snapshot()?.rows.len(), usize::from(coordinated)); }
                    if !coordinated {
                        handle.execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)").await?;
                    }
                    handle.execute_batch("INSERT INTO items VALUES(1,'first'); UPDATE items SET value='final' WHERE id=1").await?;
                    for receiver in &mut receivers {
                        let output = receiver.recv().await.context("query sink closed")?;
                        let [ChangeOperation::Added { after, .. }] = output.envelope.changes().operations() else {
                            anyhow::bail!("expected exactly one complete-transaction result")
                        };
                        assert_eq!(QueryChangeCodec::row_values_to_json(&QueryChangeCodec::decode_row(after)?.values),
                            serde_json::json!({"id":1,"value":"final"}));
                    }
                    let mut progress = consumers.read_only().subscribe()?;
                    loop {
                        let states = progress.snapshot()?;
                        if states.consumers.values().all(|state| state.checkpoints
                            .get(&SourceProgressKey::Source(drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d").expect("fixture UUID"))).is_some_and(|checkpoint| checkpoint.sequence == 1)) {
                            break;
                        }
                        progress.changed().await?;
                    }
                    handle.query("SELECT id FROM items").await?;
                    let stop = control.stop_components(GraphRevision(1), GraphSelection::All).await?;
                    anyhow::ensure!(stop.summary == OperationSummary::Completed, "{stop:?}");
                    Ok::<_, anyhow::Error>(())
                }.await;
                control.cancel();
                outcome
            })
        }).await;
        let cleanup = graph.shutdown().await;
        let (run, result) = outcome?;
        result?;
        assert!(matches!(run, Ok(()) | Err(GraphError::Cancelled)));
        cleanup?;
        assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_sqlite_factory_drives_two_real_queries_current_thread() -> Result<()> {
    factory_graph().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_sqlite_factory_drives_two_real_queries_multi_thread() -> Result<()> {
    factory_graph().await
}

async fn conservative_guards() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = owner(FIRST_ID);
    let second = owner(SECOND_ID);
    let mut one = consumer(directory.path(), first.clone(), "first/out").await?;
    let mut two = consumer(directory.path(), second.clone(), "second/out").await?;
    one.start().await?;
    two.start().await?;
    let mut settings = config(directory.path());
    settings
        .replay
        .as_mut()
        .expect("journal limits")
        .max_transactions = size(1);
    let mut source = SqliteSource::new_replayable_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        settings.clone(),
        group(&[first.clone(), second.clone()]),
    )?;
    source.start().await?;
    source.handle().execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'first')",
    ).await?;
    let input = next(&mut source).await?;
    apply(&mut one, input.clone()).await?;
    assert!(source
        .handle()
        .execute("INSERT INTO items VALUES(2,'blocked')")
        .await
        .is_err());
    assert_eq!(
        source.handle().query("SELECT id FROM items").await?.len(),
        1
    );
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?.1, 0);
    let mut removed = SqliteSource::new_replayable_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        settings.clone(),
        group(std::slice::from_ref(&first)),
    )?;
    assert!(removed
        .start()
        .await
        .expect_err("slow member removal")
        .to_string()
        .contains(&format!("cannot remove consumer {SECOND_ID}")));
    removed.stop().await?;
    source.start().await?;
    let replay = next(&mut source).await?;
    apply(&mut two, replay).await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));

    let third = owner(THIRD_ID);
    let mut three = consumer(directory.path(), third.clone(), "third/out").await?;
    three.start().await?;
    let mut late = SqliteSource::new_replayable_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        settings.clone(),
        group(&[first.clone(), second.clone(), third]),
    )?;
    assert!(
        late.start().await.is_err(),
        "late replay cannot fabricate a cursor for retired history"
    );
    late.stop().await?;
    let mut single =
        SqliteSource::new_replayable(id(SOURCE_ID), stream("changes"), settings, first)?;
    assert!(
        single.start().await.is_err(),
        "shared state cannot become single-owner state"
    );
    single.stop().await?;
    one.stop().await?;
    two.stop().await?;
    three.stop().await?;

    let single_directory = tempfile::tempdir()?;
    let first = owner(FIRST_ID);
    let mut one = consumer(single_directory.path(), first.clone(), "first/out").await?;
    one.start().await?;
    let mut single = SqliteSource::new_replayable(
        id(SOURCE_ID),
        stream("changes"),
        config(single_directory.path()),
        first.clone(),
    )?;
    single.start().await?;
    single.stop().await?;
    let mut shared = SqliteSource::new_replayable_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        config(single_directory.path()),
        group(&[first]),
    )?;
    assert!(
        shared.start().await.is_err(),
        "single-owner state cannot become shared state"
    );
    shared.stop().await?;
    one.stop().await?;
    Ok(())
}

async fn member_failures() -> Result<()> {
    for reset in [false, true] {
        let directory = tempfile::tempdir()?;
        let first = owner(FIRST_ID);
        let second = owner(SECOND_ID);
        let mut one = consumer(directory.path(), first.clone(), "first/out").await?;
        let mut two = consumer(directory.path(), second.clone(), "second/out").await?;
        one.start().await?;
        two.start().await?;
        let mut source = SqliteSource::new_replayable_with_consumers(
            id(SOURCE_ID),
            stream("changes"),
            config(directory.path()),
            group(&[first, second.clone()]),
        )?;
        source.start().await?;
        source.handle().execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'retained')",
        ).await?;
        let retained = next(&mut source).await?;
        let generation = second.snapshot().reset_generation;
        if reset {
            two.stop().await?;
            drop(two);
            two = consumer_with_text(
                directory.path(),
                second.clone(),
                "second/out",
                QueryRecoveryPolicy::AutoReset,
                "MATCH (n:items) RETURN n.id AS id, n.value AS changed",
            )
            .await?
            // This retained, committed transaction is the entire one-row database.
            .with_bootstrap(Arc::new(RetainedInitialSnapshot(retained.envelope)));
            two.start().await?;
            assert!(second.snapshot().reset_generation > generation);
        } else {
            two.stop().await?;
        }
        let error = next(&mut source)
            .await
            .expect_err("every member must remain ready");
        assert!(format!("{error:#}").contains(&format!("consumer {SECOND_ID}")));
        assert!(source
            .handle()
            .execute("INSERT INTO items VALUES(2,'unadmitted')")
            .await
            .is_err());
        source.stop().await?;
        assert_eq!(journal_state(directory.path())?.0, 1);
        assert_eq!(journal_state(directory.path())?.1, 0);
        one.stop().await?;
        two.stop().await?;
    }
    let directory = tempfile::tempdir()?;
    let volatile = owner(VOLATILE_ID);
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "native-sqlite".into(),
            id: id(VOLATILE_ID),
            query: "MATCH (n:items) RETURN n.id AS id".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("volatile/out"),
            outbox_capacity: size(4),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await?
    .with_source_progress(volatile.clone())?;
    let mut source = SqliteSource::new_replayable_with_consumers(
        id(SOURCE_ID),
        stream("changes"),
        config(directory.path()),
        group(&[volatile]),
    )?;
    query.start().await?;
    assert!(source
        .start()
        .await
        .expect_err("volatile group owner")
        .to_string()
        .contains("persistent"));
    assert!(!directory.path().join("source.db").exists());
    source.stop().await?;
    query.stop().await?;
    Ok(())
}

async fn interrupted_initialization() -> Result<()> {
    let directory = tempfile::tempdir()?;
    rusqlite::Connection::open(directory.path().join("source.db"))?.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
         INSERT INTO items VALUES(1,'small'),(2,replace(hex(zeroblob(2000)),'00','xx'));",
    )?;
    for phase in 0..3 {
        let first = owner(FIRST_ID);
        let second = owner(SECOND_ID);
        let mut settings = config(directory.path());
        if phase == 0 {
            settings.transactions.max_bytes = size(1024);
        }
        let (mut source, snapshots) = SqliteSource::coordinated_with_consumers(
            id(SOURCE_ID),
            stream("changes"),
            settings,
            group(&[first.clone(), second.clone()]),
        )?;
        let recovery = if phase == 2 {
            QueryRecoveryPolicy::AutoReset
        } else {
            QueryRecoveryPolicy::Strict
        };
        let mut one =
            consumer_with_recovery(directory.path(), first.clone(), "first/out", recovery)
                .await?
                .with_bootstrap(snapshots.for_consumer(&id(FIRST_ID))?);
        let mut two = consumer(directory.path(), second, "second/out")
            .await?
            .with_bootstrap(snapshots.for_consumer(&id(SECOND_ID))?);
        if phase < 2 {
            let error = one
                .start()
                .await
                .expect_err("partial initialization must not become success");
            if phase == 1 {
                assert!(matches!(
                    error.downcast_ref::<QueryRecoveryError>(),
                    Some(QueryRecoveryError::IncompleteBootstrap)
                ));
            }
            assert!(!first.snapshot().bootstrap_complete);
            assert!(source.handle().execute("DELETE FROM items").await.is_err());
        } else {
            tokio::try_join!(one.start(), two.start())?;
            assert_eq!(rows(&one).len(), 2);
            assert_eq!(rows(&one), rows(&two));
            source.start().await?;
            source
                .handle()
                .execute("UPDATE items SET value='recovered' WHERE id=2")
                .await?;
            let transaction = next(&mut source).await?;
            apply(&mut one, transaction.clone()).await?;
            apply(&mut two, transaction).await?;
            assert_eq!(rows(&one), rows(&two));
        }
        source.stop().await?;
        one.stop().await?;
        two.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_journal_guards_capacity_identity_and_missing_history_current_thread() -> Result<()>
{
    conservative_guards().await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_journal_guards_capacity_identity_and_missing_history_multi_thread() -> Result<()> {
    conservative_guards().await
}
#[tokio::test(flavor = "current_thread")]
async fn shared_members_fence_failure_reset_and_volatile_storage_current_thread() -> Result<()> {
    member_failures().await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_members_fence_failure_reset_and_volatile_storage_multi_thread() -> Result<()> {
    member_failures().await
}
#[tokio::test(flavor = "current_thread")]
async fn shared_initialization_errors_require_real_query_recovery_current_thread() -> Result<()> {
    interrupted_initialization().await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_initialization_errors_require_real_query_recovery_multi_thread() -> Result<()> {
    interrupted_initialization().await
}
