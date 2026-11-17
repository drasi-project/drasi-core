// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Live recovery qualification of the native PostgreSQL source: quiet
//! publications, runtime TRUNCATE, oversized row transactions and erasure
//! with activation credentials only.

use super::*;
use crate::lifecycle_ctx;
use drasi_computation_plugin_sdk::{
    lifecycle::{DisposeContext, DisposeReason, StartContext, StateIntent},
    ActivationCredentials, NativeComponent as _, NativeSource as _,
};
use drasi_computation_source_postgres::PostgresTransactionSource as PluginSource;
use drasi_postgres_common::replication::{rows::RowPosition, PostgresCdcError};

fn plugin_transactions(
    config: PostgresTransactionConfig,
    progress: &Arc<QuerySourceProgress>,
) -> Result<PluginSource> {
    PluginSource::new(
        component_id(77),
        StreamId::try_new("changes")?,
        config,
        SourceProgressReader::from(progress.clone()).read_only(),
        false,
    )
}

fn plugin_rows(
    config: PostgresTransactionConfig,
    progress: &Arc<QuerySourceProgress>,
) -> Result<PluginSource> {
    PluginSource::rows(
        component_id(77),
        StreamId::try_new("changes")?,
        config,
        SourceProgressReader::from(progress.clone()).read_only(),
        false,
        true,
    )
}

/// An ordinary (row-consuming) persistent query.
async fn row_query(
    path: &Path,
    progress: Arc<QuerySourceProgress>,
) -> Result<ContinuousQueryTransformer> {
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: progress.graph_id().into(),
            id: component_id(46),
            query: "MATCH (n:person) RETURN n.id AS id, n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("results")?,
            outbox_capacity: size(64),
        },
        provider,
    )
    .await?
    .with_source_progress(progress)
}

async fn plugin_next(source: &mut PluginSource) -> Result<OutputEnvelope> {
    tokio::time::timeout(Duration::from_secs(10), source.next())
        .await??
        .context("source exhausted")
}

fn is_progress_record(output: &OutputEnvelope) -> bool {
    output.envelope.changes().operations().is_empty()
}

/// Applies outputs until one satisfies `wanted`, which is returned (applied).
async fn apply_until(
    source: &mut PluginSource,
    query: &mut ContinuousQueryTransformer,
    mut wanted: impl FnMut(&OutputEnvelope) -> bool,
) -> Result<OutputEnvelope> {
    loop {
        let output = plugin_next(source).await?;
        let found = wanted(&output);
        apply(query, output.clone()).await?;
        if found {
            return Ok(output);
        }
    }
}

/// The LSN a query's committed cursor for the source covers.
fn covered(progress: &QuerySourceProgress, rows: bool) -> Result<u64> {
    let snapshot = progress.snapshot();
    let checkpoint = &snapshot.checkpoints[&SourceProgressKey::Source(component_id(77))];
    Ok(if rows {
        let position: RowPosition =
            serde_json::from_slice(checkpoint.source_position.as_deref().context("cursor")?)?;
        position.feedback_lsn()
    } else {
        checkpoint.sequence
    })
}

impl Database {
    /// Waits until the fixture slot has confirmed at least `position`.
    async fn wait_feedback_at_least(&self, position: u64) -> Result<u64> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let confirmed = lsn(&self.feedback().await?);
                if confirmed >= position {
                    return Result::<u64>::Ok(confirmed);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
}

/// With the selected tables quiet and the database busy, both outputs
/// commit progress records and the slot advances past the last selected
/// change, so the server stops retaining WAL behind it. Whole-transaction
/// outputs commit a keepalive's WAL end (the busy table is not published);
/// row outputs commit transactions that touch only unselected published
/// tables. Restarts resume from the committed records without loss.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_quiet_selected_tables_advance_the_slot_through_progress_records() -> Result<()> {
    for rows_mode in [false, true] {
        let database = Database::new().await?;
        database
            .client()
            .batch_execute(
                "CREATE TABLE busy (id INTEGER PRIMARY KEY);
                 CREATE TABLE audit (id INTEGER PRIMARY KEY);
                 ALTER PUBLICATION native_publication ADD TABLE audit;",
            )
            .await?;
        let path = tempfile::tempdir()?;
        let progress = progress();
        let mut query = if rows_mode {
            row_query(path.path(), progress.clone()).await?
        } else {
            super::query(path.path(), progress.clone()).await?
        };
        query.start(&lifecycle_ctx::start()).await?;
        let make = |progress: &Arc<QuerySourceProgress>| {
            let mut source = if rows_mode {
                plugin_rows(database.config.clone(), progress)?
            } else {
                plugin_transactions(database.config.clone(), progress)?
            };
            source.set_idle_progress(Duration::ZERO);
            Result::<_>::Ok(source)
        };
        let mut source = make(&progress)?;
        source.start(&mut lifecycle_ctx::native_start()).await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES (1, 'first')")
            .await?;
        apply_until(&mut source, &mut query, |output| {
            !is_progress_record(output)
        })
        .await?;
        let selected = covered(&progress, rows_mode)?;
        database.wait_feedback_at_least(selected).await?;

        // Only unselected tables change now.
        for round in 0..5 {
            database
                .client()
                .batch_execute(&format!(
                    "INSERT INTO busy SELECT generate_series({round} * 100, {round} * 100 + 99);
                     INSERT INTO audit VALUES ({round});"
                ))
                .await?;
        }
        let record = apply_until(&mut source, &mut query, is_progress_record).await?;
        assert!(is_progress_record(&record));
        let beyond = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let covered = covered(&progress, rows_mode)?;
                if covered > selected {
                    return Result::<u64>::Ok(covered);
                }
                apply(&mut query, plugin_next(&mut source).await?).await?;
            }
        })
        .await??;
        let confirmed = database.wait_feedback_at_least(beyond).await?;
        assert!(
            confirmed > selected,
            "the slot advanced past the last selected change ({confirmed:X} > {selected:X})"
        );
        source.stop(&lifecycle_ctx::native_stop()).await?;
        drop(source);

        // A new source object resumes from the committed record.
        database
            .client()
            .batch_execute("INSERT INTO person VALUES (2, 'second')")
            .await?;
        let mut source = make(&progress)?;
        source.start(&mut lifecycle_ctx::native_start()).await?;
        apply_until(&mut source, &mut query, |output| {
            !is_progress_record(output)
        })
        .await?;
        assert_eq!(
            super::rows(&query),
            vec![
                serde_json::json!({"id": 1, "name": "first"}),
                serde_json::json!({"id": 2, "name": "second"}),
            ]
        );
        source.stop(&lifecycle_ctx::native_stop()).await?;
        query.stop(&lifecycle_ctx::stop()).await?;
        database.shutdown().await?;
    }
    Ok(())
}

/// A TRUNCATE of a selected table published while the source runs fails it
/// with a typed `Unsupported`, publishes nothing of that transaction and
/// acknowledges nothing beyond the previous commit.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_runtime_truncate_fails_typed_without_partial_publication() -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = super::query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = plugin_transactions(database.config.clone(), &progress)?;
    source.start(&mut lifecycle_ctx::native_start()).await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES (1, 'kept')")
        .await?;
    apply(&mut query, plugin_next(&mut source).await?).await?;
    let before = committed(&progress);
    database.wait_feedback(before).await?;
    database
        .client()
        .batch_execute(
            "ALTER PUBLICATION native_publication SET (publish = 'insert, update, delete, truncate');
             BEGIN; INSERT INTO person VALUES (2, 'lost'); TRUNCATE person; COMMIT;",
        )
        .await?;
    let error = tokio::time::timeout(Duration::from_secs(10), source.next())
        .await?
        .expect_err("a TRUNCATE of a selected table cannot be represented");
    assert!(
        matches!(
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<PostgresCdcError>()),
            Some(PostgresCdcError::Unsupported(_))
        ),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("TRUNCATE of selected table public.person"));
    assert_eq!(committed(&progress), before);
    assert_eq!(lsn(&database.feedback().await?), before);
    assert_eq!(
        super::rows(&query),
        vec![serde_json::json!({"id": 1, "name": "kept"})]
    );
    source.stop(&lifecycle_ctx::native_stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

/// A commit beyond `maxTransactionChanges` streams in rows mode instead of
/// stopping the source, as the legacy source streamed any size. A crash in
/// the middle of it resumes after the last committed row, and the commit's
/// progress record completes the transaction so feedback reaches it.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_oversized_row_transaction_streams_and_resumes_mid_transaction() -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut config = database.config.clone();
    config.max_transaction_changes = size(2);
    let mut query = row_query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = plugin_rows(config.clone(), &progress)?;
    source.start(&mut lifecycle_ctx::native_start()).await?;
    database
        .client()
        .batch_execute(
            "BEGIN; INSERT INTO person SELECT id, 'row ' || id FROM generate_series(1, 6) AS id; COMMIT;",
        )
        .await?;
    // Three streamed rows are handled; the process then dies.
    for _ in 0..3 {
        let row = apply_until(&mut source, &mut query, |output| {
            !is_progress_record(output)
        })
        .await?;
        let position: RowPosition = serde_json::from_slice(
            &GraphChangeCodec::source_metadata(&row.envelope)?
                .context("row metadata")?
                .source_position
                .context("row position")?,
        )?;
        assert!(position.is_open(), "rows of an oversized commit stream");
    }
    source.stop(&lifecycle_ctx::native_stop()).await?;
    drop(source);
    assert_eq!(super::rows(&query).len(), 3);
    assert_eq!(
        database.feedback().await?,
        start_lsn(&database.config),
        "a streamed row never confirms its commit"
    );

    let mut source = plugin_rows(config, &progress)?;
    source.start(&mut lifecycle_ctx::native_start()).await?;
    let completion = loop {
        let output = plugin_next(&mut source).await?;
        apply(&mut query, output.clone()).await?;
        let position = covered(&progress, true)?;
        if is_progress_record(&output) && position > lsn(&start_lsn(&database.config)) {
            break position;
        }
    };
    assert_eq!(
        super::rows(&query).len(),
        6,
        "exactly the unhandled suffix replayed"
    );
    database.wait_feedback_at_least(completion).await?;
    source.stop(&lifecycle_ctx::native_stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

fn credentials() -> ActivationCredentials {
    ActivationCredentials::UsernamePassword {
        username: "postgres".into(),
        password: "postgres".into(),
    }
}

async fn managed_slots(database: &Database) -> Result<Vec<String>> {
    Ok(database
        .client()
        .query(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name LIKE 'drasi\\_%'",
            &[],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

/// A source configured without a usable login, started with host activation
/// credentials, creates its managed slot and discovers its schema. After a
/// process restart the reconstructed source is removed without starting: an
/// erase with only the dispose's activation credentials drops the slot,
/// while one without credentials cannot log in and keeps it.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_erase_after_restart_uses_only_activation_credentials() -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let mut config = coordinated_config(&database.config);
    config.user = "unprivileged".into();
    config.password = String::new();
    let progress = progress();
    let mut query = super::query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = plugin_transactions(config.clone(), &progress)?;
    source
        .start(&mut StartContext::new(
            lifecycle_ctx::scope(),
            Some(credentials()),
        ))
        .await?;
    let schema = source
        .source_schema_reader()
        .context("schema reader")?
        .describe_schema()
        .await?
        .context("schema")?;
    assert!(
        !schema.nodes.is_empty(),
        "schema discovery uses the activation's credentials"
    );
    assert_eq!(managed_slots(&database).await?.len(), 1);
    source.stop(&lifecycle_ctx::native_stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    drop(source);

    // After a restart the source is reconstructed and removed unstarted.
    let mut source = plugin_transactions(config, &progress)?;
    let erase = |attempt| {
        DisposeContext::new(
            lifecycle_ctx::scope(),
            DisposeReason::Removal,
            StateIntent::Erase,
            attempt,
        )
    };
    assert!(
        source.dispose(&erase(1)?).await.is_err(),
        "the configured login cannot authenticate"
    );
    assert_eq!(managed_slots(&database).await?.len(), 1);
    source
        .dispose(&erase(2)?.with_credentials(Some(credentials())))
        .await?;
    assert!(managed_slots(&database).await?.is_empty());
    database.shutdown().await
}

/// Each mode whose cursors must survive a restart declares that its
/// progress resource be durable. Wired to volatile progress, the graph
/// refuses the binding before the source exists or touches the database;
/// a volatile row source accepts it.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_durable_progress_requirements_are_refused_at_binding() -> Result<()> {
    use drasi_lib::DrasiLib;
    let database = Database::new().await?;
    let plugins = super::libraries::plugins(true)?;
    for (implementation, persistent, refused) in [
        (
            drasi_computation_source_postgres::POSTGRES_SOURCE,
            None,
            true,
        ),
        (
            drasi_computation_source_postgres::POSTGRES_ROWS,
            Some(true),
            true,
        ),
        (
            drasi_computation_source_postgres::POSTGRES_SHARED_TRANSACTIONS,
            None,
            true,
        ),
        (
            drasi_computation_source_postgres::POSTGRES_ROWS,
            Some(false),
            false,
        ),
    ] {
        let factory = plugins
            .source
            .factories()
            .iter()
            .find(|factory| factory.metadata().implementation.name.as_ref() == implementation)
            .context("PostgreSQL factory")?
            .clone();
        let shared =
            implementation == drasi_computation_source_postgres::POSTGRES_SHARED_TRANSACTIONS;
        let owner = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            component_id(46),
        )?);
        let (dependency, progress) = if shared {
            (
                "consumer_progress",
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(ConsumerProgressSet::new(
                        ComponentBatch::GRAPH_ID,
                        component_id(77),
                        [owner],
                    )?),
                ),
            )
        } else {
            (
                "source_progress",
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(QuerySourceProgressResource(owner)),
                ),
            )
        };
        let progress_id = ResourceId::try_new("progress")?;
        let settings = ResourceId::try_new("settings")?;
        let mut configuration: BTreeMap<Arc<str>, ConfigurationValue> = BTreeMap::from([(
            "stream".into(),
            ConfigurationValue::Literal(serde_json::json!("changes")),
        )]);
        if let Some(persistent) = persistent {
            configuration.insert(
                "persistent".into(),
                ConfigurationValue::Literal(serde_json::json!(persistent)),
            );
        }
        for (key, value) in settings_fields(&database.config, &settings) {
            configuration.insert(key.into(), value);
        }
        let batch = ComponentBatch::builder()
            .declare_resource(ResourceSpecification {
                id: progress_id.clone(),
                role: ResourceRole::Checkpoint,
                ownership: ResourceOwnership::Borrowed,
                binding: "progress".into(),
            })?
            .provide_resource(
                progress_id.clone(),
                progress.with_capabilities(ResourceCapabilities::VOLATILE),
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
                        serde_json::json!(database.config.password),
                    )))),
                ),
            )?
            .component(
                ComponentName::try_new("source")?,
                ComponentSpecification {
                    descriptor: factory.metadata().descriptor(component_id(77))?,
                    role: ComponentRole::Source,
                    completion: None,
                    implementation: factory.descriptor().implementation.clone(),
                    configuration_version: 1,
                    configuration,
                    dependencies: BTreeMap::from([(dependency.into(), vec![progress_id])]),
                },
                factory,
            )
            .bind_stream(
                endpoint(&component_id(77), "out"),
                StreamId::try_new("changes")?,
            )
            .build()?;
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_id("postgres-native-binding")
            .with_components(batch)
            .build()
            .await?;
        let _ = core.start().await;
        let node = core.computation_control()?.observed().components[&component_id(77)].clone();
        if refused {
            let failure = node.failure.context("binding failure")?;
            assert_eq!(failure.phase, FailurePhase::Binding, "{implementation}");
            assert!(
                matches!(
                    failure.cause.underlying(),
                    GraphError::Incompatible {
                        source: Incompatibility::ResourceDurability { .. },
                        ..
                    }
                ),
                "{implementation}: {failure:?}"
            );
            assert_eq!(
                database
                    .client()
                    .query_one("SELECT count(*) FROM pg_stat_replication", &[],)
                    .await?
                    .get::<_, i64>(0),
                0,
                "{implementation} never connected"
            );
        } else {
            assert!(
                !node
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.phase == FailurePhase::Binding),
                "{node:?}"
            );
        }
        core.shutdown().await?;
    }
    database.shutdown().await
}
