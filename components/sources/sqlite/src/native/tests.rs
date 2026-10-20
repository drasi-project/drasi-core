// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use tokio::time::timeout;

fn config(output: SqliteOutput) -> SqliteConfig {
    SqliteConfig {
        path: None,
        tables: Vec::new(),
        table_keys: Vec::new(),
        output,
        replay: None,
        transactions: SourceTransactionLimits {
            max_changes: NonZeroUsize::new(32).unwrap(),
            max_bytes: NonZeroUsize::new(1 << 20).unwrap(),
            max_duration_ms: NonZeroU64::new(5000).unwrap(),
        },
        max_sql_bytes: NonZeroUsize::new(1 << 16).unwrap(),
        command_capacity: NonZeroUsize::new(4).unwrap(),
        output_capacity: NonZeroUsize::new(4).unwrap(),
        shutdown_timeout_ms: NonZeroU64::new(5000).unwrap(),
    }
}
fn source(config: SqliteConfig) -> Result<SqliteSource> {
    SqliteSource::new(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        config,
    )
}
async fn next(source: &mut SqliteSource) -> Result<OutputEnvelope> {
    timeout(Duration::from_secs(2), source.next())
        .await??
        .context("source ended")
}
async fn no_output(source: &mut SqliteSource) {
    assert!(timeout(Duration::from_millis(20), source.next())
        .await
        .is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn shared_factory_requires_exact_scope_resources_and_exclusive_progress() -> Result<()> {
    use std::collections::BTreeMap;
    let directory = tempfile::tempdir()?;
    let mut settings = config(SqliteOutput::Transactions);
    settings.path = Some(directory.path().join("source.db"));
    settings.replay = Some(SqliteReplayConfig {
        max_transactions: NonZeroUsize::new(8).unwrap(),
        max_bytes: NonZeroUsize::new(1 << 20).unwrap(),
    });
    let id = |name| ComponentId::try_new(name).unwrap();
    let owner = Arc::new(QuerySourceProgress::new("graph", id("query"))?);
    let consumers = Arc::new(ConsumerProgressSet::new(
        "graph",
        id("sqlite"),
        [owner.clone()],
    )?);
    let (producer, snapshots) = SqliteSource::coordinated_with_consumers(
        id("sqlite"),
        StreamId::try_new("changes")?,
        settings.clone(),
        consumers.clone(),
    )?;
    assert_eq!(
        producer.recovery_contract(),
        producer.recovery_contract(),
        "cursor policy must be stable"
    );
    assert!(
        !settings.path.as_ref().unwrap().exists(),
        "construction must not touch SQLite"
    );
    assert!(snapshots.for_consumer(&id("missing")).is_err());
    let snapshot = snapshots.for_consumer(&id("query"))?;
    snapshot.validate_query_scope("instance", "graph", &id("query"))?;
    assert!(snapshot
        .validate_query_scope("instance", "wrong", &id("query"))
        .is_err());
    assert!(snapshot
        .validate_query_scope("instance", "graph", &id("other"))
        .is_err());
    let substitute = Arc::new(ConsumerProgressSet::new("graph", id("sqlite"), [owner])?);
    let substitute_source = SqliteSource::new_replayable_with_consumers(
        id("sqlite"),
        StreamId::try_new("changes")?,
        settings.clone(),
        substitute,
    )?;
    assert!(snapshots.validate_source(&substitute_source).is_err());
    let client = Arc::new(SqliteClientResource::new(id("sqlite"), settings.clone())?);
    let factory = SqliteSourceFactory::default();
    let mut specification = ComponentSpecification {
        descriptor: producer.descriptor().clone(),
        role: ComponentRole::Source,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: producer
            .configuration()?
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| {
                (
                    Arc::from(key.as_str()),
                    ConfigurationValue::Literal(value.clone()),
                )
            })
            .collect(),
        dependencies: BTreeMap::from([
            (Arc::from("client"), vec![ResourceId::try_new("client")?]),
            (
                Arc::from("consumer_progress"),
                vec![ResourceId::try_new("consumers")?],
            ),
            (
                Arc::from("consumer_snapshots"),
                vec![ResourceId::try_new("snapshots")?],
            ),
        ]),
    };
    let mut declarations = BTreeMap::new();
    let mut resources = BTreeMap::new();
    for (name, role, handle) in [
        (
            "client",
            ResourceRole::Component,
            ResourceHandle::new(ResourceRole::Component, client),
        ),
        (
            "consumers",
            ResourceRole::Checkpoint,
            ResourceHandle::new(ResourceRole::Checkpoint, consumers),
        ),
        (
            "snapshots",
            ResourceRole::Bootstrap,
            ResourceHandle::new(ResourceRole::Bootstrap, snapshots),
        ),
    ] {
        let resource = ResourceId::try_new(name)?;
        declarations.insert(
            resource.clone(),
            ResourceSpecification {
                id: resource.clone(),
                role,
                ownership: ResourceOwnership::Borrowed,
                binding: name.into(),
            },
        );
        resources.insert(resource, handle);
    }
    factory.validate(&specification)?;
    factory.validate_scope("graph", &specification, &declarations, &resources)?;
    assert!(factory
        .validate_scope("wrong", &specification, &declarations, &resources)
        .is_err());
    specification.dependencies.insert(
        "source_progress".into(),
        vec![ResourceId::try_new("single")?],
    );
    assert!(factory.validate(&specification).is_err());
    specification.dependencies.remove("source_progress");
    specification.dependencies.insert(
        "snapshot".into(),
        vec![ResourceId::try_new("single-snapshot")?],
    );
    assert!(factory.validate(&specification).is_err());
    Ok(())
}

#[test]
fn shared_cursor_order_validates_journal_epochs_and_logical_positions() -> Result<()> {
    let source = ComponentId::try_new("source")?;
    let policy = journal::position_policy(&source)?;
    let epoch = uuid::Uuid::new_v4();
    let first = journal::identity(epoch, 2)?;
    let next = journal::identity(epoch, 10)?;
    assert!(policy.is_after(&next, &first)?);
    assert!(!policy.is_after(&first, &next)?);
    assert!(!policy.is_after(&first, &first)?);
    assert!(policy
        .is_after(&next, &journal::identity(uuid::Uuid::new_v4(), 2)?)
        .is_err());
    assert!(policy.is_after(b"invalid", &first).is_err());
    assert!(policy
        .is_after(br#"{"version":2,"epoch":"invalid","sequence":1}"#, &first)
        .is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_configuration_and_factory_reject_incompatible_bounds_and_resources() -> Result<()> {
    for invalid in [
        "commands",
        "outputs",
        "sql",
        "duration",
        "duplicate",
        "reserved",
    ] {
        let mut settings = config(SqliteOutput::Transactions);
        match invalid {
            "commands" => settings.command_capacity = NonZeroUsize::new(1).expect("positive"),
            "outputs" => {
                settings.output_capacity =
                    NonZeroUsize::new(tokio::sync::Semaphore::MAX_PERMITS + 1).expect("positive")
            }
            "sql" => {
                settings.max_sql_bytes = NonZeroUsize::new(i32::MAX as usize + 1).expect("positive")
            }
            "duration" => {
                settings.transactions.max_duration_ms =
                    NonZeroU64::new(i32::MAX as u64 + 1).expect("positive")
            }
            "duplicate" => settings.tables = vec!["items".into(), "items".into()],
            "reserved" => settings.tables = vec!["__DRASI_private".into()],
            _ => unreachable!(),
        }
        assert!(settings.validate().is_err(), "{invalid}");
        assert!(source(settings).is_err(), "{invalid}");
    }
    let factory = SqliteSourceFactory::default();
    let settings = config(SqliteOutput::Transactions);
    let specification = ComponentSpecification {
        descriptor: SqliteSource::describe(ComponentId::try_new("sqlite")?, settings.output)?,
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
                ConfigurationValue::Literal(serde_json::to_value(&settings)?),
            ),
        ]),
        dependencies: std::collections::BTreeMap::from([(
            Arc::from("client"),
            vec![ResourceId::try_new("client")?],
        )]),
    };
    factory.validate(&specification)?;
    for invalid in [
        "schema",
        "progress",
        "snapshot",
        "flag",
        "rest-shape",
        "rest-host",
    ] {
        let mut specification = specification.clone();
        match invalid {
            "schema" => {
                specification.descriptor =
                    SqliteSource::describe(ComponentId::try_new("sqlite")?, SqliteOutput::Changes)?
            }
            "progress" => {
                specification.dependencies.insert(
                    Arc::from("source_progress"),
                    vec![ResourceId::try_new("progress")?],
                );
            }
            "snapshot" => {
                specification.configuration.insert(
                    Arc::from("coordinated_snapshot"),
                    ConfigurationValue::Literal(true.into()),
                );
            }
            "flag" => {
                specification.configuration.insert(
                    Arc::from("coordinated_snapshot"),
                    ConfigurationValue::Literal("true".into()),
                );
            }
            "rest-shape" => {
                specification.configuration.insert(
                    Arc::from("rest_api"),
                    ConfigurationValue::Literal("not-an-object".into()),
                );
            }
            "rest-host" => {
                specification.configuration.insert(
                    Arc::from("rest_api"),
                    ConfigurationValue::Literal(serde_json::json!({"host":"","port":0})),
                );
            }
            _ => unreachable!(),
        }
        assert!(factory.validate(&specification).is_err(), "{invalid}");
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paired_snapshot_requires_the_actual_source_configuration_and_progress_owner() -> Result<()>
{
    let owner = || -> Result<_> {
        Ok(Arc::new(QuerySourceProgress::new(
            "test",
            ComponentId::try_new("query")?,
        )?))
    };
    let progress = owner()?;
    let mut settings = config(SqliteOutput::Transactions);
    settings.path = Some("not-opened-by-construction.db".into());
    settings.replay = Some(SqliteReplayConfig {
        max_transactions: NonZeroUsize::new(8).expect("positive"),
        max_bytes: NonZeroUsize::new(8 << 20).expect("positive"),
    });
    let make = |settings, progress| {
        SqliteSource::new_replayable(
            ComponentId::try_new("sqlite")?,
            StreamId::try_new("changes")?,
            settings,
            progress,
        )
    };
    let (producer, snapshot) = SqliteSource::coordinated(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        settings.clone(),
        progress.clone(),
    )?;
    snapshot.validate_source(&producer)?;
    snapshot.validate_source(&make(settings.clone(), progress.clone())?)?;
    assert!(snapshot
        .validate_source(&make(settings.clone(), owner()?)?)
        .is_err());
    let mut changed = settings.clone();
    changed.tables = vec!["items".into()];
    assert!(snapshot
        .validate_source(&make(changed.clone(), progress.clone())?)
        .is_err());
    let mismatched = Arc::new(SqliteClientResource::new(
        ComponentId::try_new("sqlite")?,
        changed,
    )?);
    assert!(make(settings.clone(), progress.clone())?
        .with_client_resource(mismatched)
        .is_err());
    let client = Arc::new(SqliteClientResource::new(
        ComponentId::try_new("sqlite")?,
        settings.clone(),
    )?);
    let bound = producer.with_client_resource(client.clone())?;
    assert!(make(settings.clone(), progress.clone())?
        .with_client_resource(client.clone())
        .is_err());
    drop(bound);
    make(settings, progress)?.with_client_resource(client)?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rest_sql_handles_are_fenced_to_their_original_database_worker() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut settings = config(SqliteOutput::Changes);
    settings.path = Some(directory.path().join("source.db"));
    let mut source = source(settings)?;
    source.start().await?;
    let live = source.handle();
    let bound = live.bind_to_current_worker().await?;
    bound
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    source.stop().await?;
    source.start().await?;
    assert!(bound.execute("INSERT INTO items VALUES(1)").await.is_err());
    live.execute("INSERT INTO items VALUES(2)").await?;
    assert_eq!(
        serde_json::to_value(live.query("SELECT id FROM items").await?)?,
        serde_json::json!([{"id":2}])
    );
    let output = next(&mut source).await?;
    assert_eq!(GraphChangeCodec::decode_changes(&output.envelope)?.len(), 1);
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn unexpected_rest_worker_failure_is_a_native_source_failure() -> Result<()> {
    let mut source =
        source(config(SqliteOutput::Changes))?.with_rest_api(crate::RestApiConfig {
            host: "127.0.0.1".into(),
            port: 0,
        })?;
    source.start().await?;
    source
        .rest_worker
        .get_mut()
        .as_ref()
        .context("REST owner")?
        .abort();
    let error = source
        .next()
        .await
        .expect_err("REST failure must not leave a healthy idle source");
    assert!(format!("{error:#}").contains("REST worker"), "{error:#}");
    assert!(source.bound_rest_address().is_none());
    assert!(source.start().await.is_err());
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_key_configuration_is_validated_without_changing_default_bindings() -> Result<()> {
    let settings = config(SqliteOutput::Changes);
    let serialized = serde_json::to_value(&settings)?;
    assert!(serialized.get("table_keys").is_none());
    assert_eq!(
        serde_json::from_value::<SqliteConfig>(serialized)?,
        settings
    );
    let source_id = ComponentId::try_new("sqlite")?;
    let output = StreamId::try_new("changes")?;
    let owner: SourceProgressReader = Arc::new(QuerySourceProgress::new(
        "test",
        ComponentId::try_new("query")?,
    )?)
    .into();
    let owner = owner.read_only();
    let original = serde_json::to_string(&(
        1u32,
        "sqlite",
        "changes",
        "test",
        "query",
        Vec::<String>::new(),
    ))?;
    assert_eq!(
        journal::binding(&settings, &source_id, &output, &owner, false)?,
        original
    );
    assert_eq!(
        journal::binding(&settings, &source_id, &output, &owner, true)?,
        serde_json::to_string(&("coordinated-v1", &original))?,
    );
    let mut keyed = settings.clone();
    keyed.table_keys = vec![crate::TableKeyConfig {
        table: "items".into(),
        key_columns: vec!["code".into()],
    }];
    keyed.validate()?;
    assert_ne!(
        journal::binding(&keyed, &source_id, &output, &owner, false)?,
        original
    );
    for invalid in [
        "empty-table",
        "reserved-table",
        "duplicate-table",
        "empty-key",
        "duplicate-key",
        "unselected",
    ] {
        let mut config = keyed.clone();
        match invalid {
            "empty-table" => config.table_keys[0].table.clear(),
            "reserved-table" => config.table_keys[0].table = "__drasi_reserved".into(),
            "duplicate-table" => config.table_keys.push(config.table_keys[0].clone()),
            "empty-key" => config.table_keys[0].key_columns.clear(),
            "duplicate-key" => config.table_keys[0].key_columns.push("CODE".into()),
            "unselected" => config.tables = vec!["other".into()],
            _ => unreachable!(),
        }
        assert!(config.validate().is_err(), "{invalid}");
        assert!(source(config).is_err(), "{invalid}");
    }
    Ok(())
}

async fn configured_keys() -> Result<()> {
    use drasi_core::models::SourceChange;
    for mode in [SqliteOutput::Changes, SqliteOutput::Transactions] {
        for primary_key in [false, true] {
            let mut settings = config(mode);
            settings.table_keys = vec![crate::TableKeyConfig {
                table: "items".into(),
                key_columns: vec!["CODE".into()],
            }];
            let limits = settings.transactions;
            let mut source = source(settings)?;
            source.start().await?;
            let handle = source.handle();
            let result = async {
                let primary_key = if primary_key { "PRIMARY KEY" } else { "" };
                handle
                    .execute_batch(format!(
                    "CREATE TABLE items(id INTEGER {primary_key}, code TEXT UNIQUE, value TEXT);
                     INSERT INTO items VALUES(1,'first:key','one');"
                ))
                    .await?;
                let decode = |output: OutputEnvelope| -> Result<Vec<SourceChange>> {
                    match mode {
                        SqliteOutput::Changes => {
                            Ok(GraphChangeCodec::decode_changes(&output.envelope)?)
                        }
                        SqliteOutput::Transactions => {
                            Ok(SourceTransactionCodec::decode(&output.envelope, limits)?
                                .into_changes())
                        }
                    }
                };
                let inserted = decode(next(&mut source).await?)?;
                assert_eq!(inserted.len(), 1);
                let reference = inserted[0].get_reference().clone();
                let key: serde_json::Value = serde_json::from_str(
                    reference
                        .element_id
                        .strip_prefix("sqlite:v1:")
                        .context("typed SQLite identity")?,
                )?;
                assert_eq!(
                    key,
                    serde_json::json!(["items", {"code":["text","first:key"]}])
                );
                handle.execute("UPDATE items SET id=2").await?;
                let updated = decode(next(&mut source).await?)?;
                assert!(matches!(&updated[..],
                    [SourceChange::Update { element }] if element.get_reference() == &reference));
                handle.execute("UPDATE items SET code='second:key'").await?;
                let changed = decode(next(&mut source).await?)?;
                assert!(matches!(&changed[..], [
                    SourceChange::Delete { metadata },
                    SourceChange::Insert { element },
                ] if metadata.reference == reference && element.get_reference() != &reference));
                assert!(handle
                    .execute("UPDATE items SET code=NULL, value='lost'")
                    .await
                    .is_err());
                assert_eq!(
                    serde_json::to_value(handle.query("SELECT code,value FROM items").await?)?,
                    serde_json::json!([{"code":"second:key","value":"one"}])
                );
                no_output(&mut source).await;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let cleanup = source.stop().await;
            result?;
            cleanup?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn all_shadowed_rowid_aliases_require_explicit_keys() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shadowed.db");
    rusqlite::Connection::open(&path)?
        .execute_batch("CREATE TABLE items(id INTEGER, rowid TEXT, _rowid_ TEXT, oid TEXT);")?;
    let mut settings = config(SqliteOutput::Changes);
    settings.path = Some(path);
    let mut rejected = source(settings.clone())?;
    let error = rejected
        .start()
        .await
        .expect_err("rowid must not be confused with a user column");
    assert!(
        format!("{error:#}").contains("configure explicit key columns"),
        "{error:#}"
    );
    rejected.stop().await?;
    settings.table_keys = vec![crate::TableKeyConfig {
        table: "items".into(),
        key_columns: vec!["id".into()],
    }];
    let mut source = source(settings)?;
    source.start().await?;
    source
        .handle()
        .execute("INSERT INTO items VALUES(1,'rowid','_rowid_','oid')")
        .await?;
    let changes = GraphChangeCodec::decode_changes(&next(&mut source).await?.envelope)?;
    assert_eq!(changes.len(), 1);
    assert!(changes[0]
        .get_reference()
        .element_id
        .starts_with("sqlite:v1:"));
    source.stop().await?;
    Ok(())
}

async fn keyless_rowids() -> Result<()> {
    use drasi_core::models::{Element, SourceChange};
    for mode in [SqliteOutput::Changes, SqliteOutput::Transactions] {
        let settings = config(mode);
        let limits = settings.transactions;
        let mut source = source(settings)?;
        source.start().await?;
        let handle = source.handle();
        let result = async {
            handle.execute_batch(
                "CREATE TABLE items(id INTEGER, rowid TEXT, _rowid_ TEXT, value TEXT);
                 INSERT INTO items(oid,id,rowid,_rowid_,value) VALUES(-7,1,'user-rowid','user-alias','before');",
            ).await?;
            let decode = |output: OutputEnvelope| -> Result<Vec<SourceChange>> {
                match mode {
                    SqliteOutput::Changes => Ok(GraphChangeCodec::decode_changes(&output.envelope)?),
                    SqliteOutput::Transactions => Ok(SourceTransactionCodec::decode(&output.envelope, limits)?.into_changes()),
                }
            };
            let inserted = decode(next(&mut source).await?)?;
            let [SourceChange::Insert { element: Element::Node { metadata, properties } }] = &inserted[..] else {
                anyhow::bail!("expected one native rowid insert");
            };
            assert_eq!(metadata.reference.element_id.as_ref(), "sqlite:rowid:v1:[\"items\",-7]");
            assert_eq!(properties.get("rowid").map(serde_json::Value::from), Some("user-rowid".into()));
            assert_eq!(properties.get("_rowid_").map(serde_json::Value::from), Some("user-alias".into()));
            assert!(properties.get("oid").is_none(), "physical rowid must not become a synthetic property");
            let first = metadata.reference.clone();
            handle.execute("UPDATE items SET id=2,value='after'").await?;
            let updated = decode(next(&mut source).await?)?;
            assert!(matches!(&updated[..], [SourceChange::Update { element }] if element.get_reference() == &first));
            handle.execute("UPDATE items SET oid=42").await?;
            let moved = decode(next(&mut source).await?)?;
            assert!(matches!(&moved[..], [
                SourceChange::Delete { metadata },
                SourceChange::Insert { element },
            ] if metadata.reference == first && element.get_reference().element_id.as_ref() == "sqlite:rowid:v1:[\"items\",42]"));
            handle.execute("DELETE FROM items").await?;
            let deleted = decode(next(&mut source).await?)?;
            assert!(matches!(&deleted[..], [SourceChange::Delete { metadata }]
                if metadata.reference.element_id.as_ref() == "sqlite:rowid:v1:[\"items\",42]"));
            Ok::<_, anyhow::Error>(())
        }.await;
        let cleanup = source.stop().await;
        result?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_rowids_preserve_keyless_rows_and_shadowed_properties_current_thread() -> Result<()>
{
    keyless_rowids().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_rowids_preserve_keyless_rows_and_shadowed_properties_multi_thread() -> Result<()> {
    keyless_rowids().await
}

#[tokio::test(flavor = "current_thread")]
async fn configured_keys_validate_actual_columns_before_admission() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("input.db");
    let database = rusqlite::Connection::open(&path)?;
    database.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
        INSERT INTO items VALUES(1,'retained');",
    )?;
    drop(database);
    let mut settings = config(SqliteOutput::Changes);
    settings.path = Some(path.clone());
    settings.table_keys = vec![crate::TableKeyConfig {
        table: "items".into(),
        key_columns: vec!["missing".into()],
    }];
    let mut source = source(settings)?;
    let error = source
        .start()
        .await
        .expect_err("unknown key column cannot be ignored");
    assert!(
        format!("{error:#}").contains("key column 'missing'"),
        "{error:#}"
    );
    assert!(source
        .handle()
        .execute("UPDATE items SET value='lost'")
        .await
        .is_err());
    source.stop().await?;
    let retained: String = rusqlite::Connection::open(&path)?.query_row(
        "SELECT value FROM items WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(retained, "retained");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_keys_drive_live_identity_and_key_changes_current_thread() -> Result<()> {
    configured_keys().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_keys_drive_live_identity_and_key_changes_multi_thread() -> Result<()> {
    configured_keys().await
}

#[tokio::test(flavor = "current_thread")]
async fn both_native_modes_publish_only_after_the_isolated_scope_commits() -> Result<()> {
    for mode in [SqliteOutput::Changes, SqliteOutput::Transactions] {
        let config = config(mode);
        let limits = config.transactions;
        let mut source = source(config)?;
        source.start().await?;
        let handle = source.handle();
        handle
            .execute("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        let (entered, started) = oneshot::channel();
        let (finish, resume) = oneshot::channel();
        let mut transaction = Box::pin(handle.transaction(|tx| async move {
            tx.execute("INSERT INTO items VALUES(1,'initial')").await?;
            tx.execute("UPDATE items SET value='final' WHERE id=1")
                .await?;
            entered.send(()).unwrap();
            resume.await?;
            Ok(())
        }));
        tokio::select! { result = &mut transaction => panic!("{result:?}"), result = started => result? }
        no_output(&mut source).await;
        let other = handle.query("SELECT value FROM items");
        tokio::pin!(other);
        assert!(timeout(Duration::from_millis(20), &mut other)
            .await
            .is_err());
        finish.send(()).unwrap();
        transaction.await?;
        assert_eq!(
            serde_json::to_value(other.await?)?,
            serde_json::json!([{"value":"final"}])
        );
        let output = next(&mut source).await?;
        let changes = match mode {
            SqliteOutput::Changes => GraphChangeCodec::decode_changes(&output.envelope)?,
            SqliteOutput::Transactions => {
                SourceTransactionCodec::decode(&output.envelope, limits)?.into_changes()
            }
        };
        assert_eq!(changes.len(), 2);
        source.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_rolls_back_and_revokes_escaped_handles_before_other_work() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let (entered, captured) = oneshot::channel();
    let mut transaction = Box::pin(handle.transaction(|tx| async move {
        tx.execute("INSERT INTO items VALUES(1)").await?;
        entered.send(tx).ok().context("capture closed")?;
        std::future::pending::<Result<()>>().await
    }));
    let tx = tokio::select! { result = &mut transaction => panic!("{result:?}"), result = captured => result? };
    let mut pending = Vec::new();
    for _ in 0..4 {
        let mut query = Box::pin(handle.query("SELECT id FROM items"));
        assert!(timeout(Duration::from_millis(5), &mut query).await.is_err());
        pending.push(query);
    }
    assert_eq!(source.commands.read().await.as_ref().unwrap().capacity(), 0);
    drop(pending);
    drop(transaction);
    assert!(tx.execute("INSERT INTO items VALUES(2)").await.is_err());
    assert!(handle.query("SELECT id FROM items").await?.is_empty());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn caught_statement_errors_and_failed_commits_never_publish_or_leave_rows() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY);
         CREATE TABLE children(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES items(id) DEFERRABLE INITIALLY DEFERRED);"
    ).await?;
    let result = handle
        .transaction(|tx| async move {
            tx.execute("INSERT INTO items VALUES(1)").await?;
            assert!(tx
                .execute("INSERT OR FAIL INTO items VALUES(2),(2)")
                .await
                .is_err());
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    assert!(handle
        .execute("INSERT INTO children VALUES(1,999)")
        .await
        .is_err());
    assert!(handle.query("SELECT * FROM children").await?.is_empty());
    no_output(&mut source).await;
    handle.execute("INSERT INTO items VALUES(3)").await?;
    next(&mut source).await?;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn parsed_savepoints_and_key_changes_preserve_exact_committed_changes() -> Result<()> {
    let config = config(SqliteOutput::Transactions);
    let limits = config.transactions;
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id TEXT PRIMARY KEY, value TEXT)")
        .await?;
    handle
        .transaction(|tx| async move {
            tx.execute_batch(
                "INSERT INTO items VALUES('a','one');
             /* quoted identifier */ SAVEPOINT \"outer point\";
             INSERT INTO items VALUES('b','two');
             SAVEPOINT \"inner point\";
             INSERT INTO items VALUES('c','three');
             RELEASE \"outer point\";
             SAVEPOINT \"outer point\";
             INSERT INTO items VALUES('d','four');
             ROLLBACK TO \"outer point\";
             RELEASE \"outer point\";
             UPDATE items SET id='new:a' WHERE id='a';",
            )
            .await?;
            Ok(())
        })
        .await?;
    assert_eq!(handle.query("SELECT id FROM items").await?.len(), 3);
    let output = next(&mut source).await?;
    let changes = SourceTransactionCodec::decode(&output.envelope, limits)?.into_changes();
    assert_eq!(changes.len(), 5);
    assert!(matches!(
        &changes[3],
        drasi_core::models::SourceChange::Delete { .. }
    ));
    assert!(matches!(
        &changes[4],
        drasi_core::models::SourceChange::Insert { .. }
    ));
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn authorizer_denies_raw_boundaries_and_query_mutations() -> Result<()> {
    let mut source = source(config(SqliteOutput::Changes))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    for sql in [
        "BEGIN",
        "END",
        "COMMIT",
        "ROLLBACK",
        "/* comment */ END TRANSACTION",
        "PRAGMA foreign_keys=OFF",
        "ATTACH ':memory:' AS extra",
    ] {
        assert!(
            handle
                .execute_batch(format!("INSERT INTO items VALUES(1); {sql};"))
                .await
                .is_err(),
            "{sql}"
        );
        assert!(handle.query("SELECT * FROM items").await?.is_empty());
    }
    assert!(handle
        .query("INSERT INTO items VALUES(1) RETURNING id")
        .await
        .is_err());
    assert!(handle
        .query("SELECT 1; INSERT INTO items VALUES(1)")
        .await
        .is_err());
    assert!(handle.query("SAVEPOINT outside_scope").await.is_err());
    assert!(handle
        .transaction(|tx| async move {
            tx.execute("INSERT INTO items VALUES(1)").await?;
            assert!(tx.query("SAVEPOINT query_escape").await.is_err());
            Ok(())
        })
        .await
        .is_err());
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stop_rolls_back_a_scope_and_old_handles_cannot_reach_replacement() -> Result<()> {
    let directory = tempfile::TempDir::new()?;
    let mut config = config(SqliteOutput::Transactions);
    config.path = Some(directory.path().join("source.db"));
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let (entered, captured) = oneshot::channel();
    let (finish, resume) = oneshot::channel();
    let mut transaction = Box::pin(handle.transaction(|tx| async move {
        tx.execute("INSERT INTO items VALUES(1)").await?;
        entered.send(tx.clone()).ok().context("capture closed")?;
        resume.await?;
        tx.execute("INSERT INTO items VALUES(2)").await?;
        Ok(())
    }));
    let tx = tokio::select! { result = &mut transaction => panic!("{result:?}"), result = captured => result? };
    source.stop().await?;
    source.start().await?;
    assert!(tx.execute("INSERT INTO items VALUES(3)").await.is_err());
    finish.send(()).unwrap();
    assert!(transaction.await.is_err());
    assert!(handle.query("SELECT id FROM items").await?.is_empty());
    handle.execute("INSERT INTO items VALUES(4)").await?;
    next(&mut source).await?;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_capture_is_rejected_atomically_and_increased_limits_allow_retry() -> Result<()> {
    let directory = tempfile::TempDir::new()?;
    let mut config = config(SqliteOutput::Transactions);
    config.path = Some(directory.path().join("source.db"));
    config.transactions.max_changes = NonZeroUsize::new(1).unwrap();
    let mut first = source(config.clone())?;
    first.start().await?;
    first
        .handle()
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    assert!(first
        .handle()
        .execute("INSERT INTO items VALUES(1),(2)")
        .await
        .is_err());
    assert!(first
        .handle()
        .query("SELECT * FROM items")
        .await?
        .is_empty());
    no_output(&mut first).await;
    first.stop().await?;
    config.transactions.max_changes = NonZeroUsize::new(2).unwrap();
    let mut second = source(config)?;
    second.start().await?;
    second
        .handle()
        .execute("INSERT INTO items VALUES(1),(2)")
        .await?;
    next(&mut second).await?;
    second.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn local_validation_errors_also_abort_a_scope_even_when_caught() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    for sql in ["SELECT \0".to_string(), "x".repeat(65537)] {
        assert!(handle
            .transaction(|tx| async move {
                tx.execute("INSERT INTO items VALUES(1)").await?;
                assert!(tx.execute(sql).await.is_err());
                Ok(())
            })
            .await
            .is_err());
        assert!(handle.query("SELECT * FROM items").await?.is_empty());
    }
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn idle_scopes_and_expensive_queries_obey_real_execution_deadlines() -> Result<()> {
    let mut config = config(SqliteOutput::Transactions);
    config.transactions.max_duration_ms = NonZeroU64::new(60).unwrap();
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let (entered, ready) = oneshot::channel();
    let (finish, resume) = oneshot::channel();
    let mut transaction = Box::pin(handle.transaction(|tx| async move {
        tx.execute("INSERT INTO items VALUES(1)").await?;
        entered.send(()).unwrap();
        resume.await?;
        Ok(())
    }));
    tokio::select! { result = &mut transaction => panic!("{result:?}"), result = ready => result? }
    tokio::time::sleep(Duration::from_millis(90)).await;
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    finish.send(()).unwrap();
    assert!(transaction
        .await
        .unwrap_err()
        .to_string()
        .contains("deadline"));
    let began = tokio::time::Instant::now();
    assert!(timeout(Duration::from_secs(2), handle.query(
        "WITH RECURSIVE numbers(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM numbers WHERE x<1000000000) SELECT sum(x) FROM numbers"
    )).await?.is_err());
    assert!(began.elapsed() >= Duration::from_millis(60));
    let began = tokio::time::Instant::now();
    let error = timeout(
        Duration::from_secs(2),
        handle.transaction(|_| std::future::pending::<Result<()>>()),
    )
    .await?
    .unwrap_err();
    assert!(error.to_string().contains("deadline"));
    assert!(began.elapsed() >= Duration::from_millis(60));
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_keys_cover_without_rowid_composites_blobs_and_cascades() -> Result<()> {
    let config = config(SqliteOutput::Transactions);
    let limits = config.transactions;
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle.execute_batch(
        "CREATE TABLE items(a TEXT,b TEXT,PRIMARY KEY(a,b)) WITHOUT ROWID;
         CREATE TABLE blobs(id PRIMARY KEY);
         CREATE TABLE parent(id INTEGER PRIMARY KEY);
         CREATE TABLE child(id INTEGER PRIMARY KEY,parent INTEGER REFERENCES parent(id) ON DELETE CASCADE);"
    ).await?;
    handle.execute_batch(
        "INSERT INTO items VALUES('a:b','c'),('a','b:c');
         INSERT INTO blobs VALUES(X'61'),('YQ==');
         INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1); DELETE FROM parent WHERE id=1;"
    ).await?;
    let changes =
        SourceTransactionCodec::decode(&next(&mut source).await?.envelope, limits)?.into_changes();
    assert_eq!(changes.len(), 8);
    let references: std::collections::BTreeSet<_> = changes[..4]
        .iter()
        .map(|change| change.get_reference().element_id.to_string())
        .collect();
    assert_eq!(
        references.len(),
        4,
        "composite and BLOB/text keys must not collide"
    );
    assert!(handle.query("SELECT * FROM child").await?.is_empty());
    assert!(handle
        .query("SELECT 1 AS duplicate,2 AS duplicate")
        .await
        .is_err());
    assert!(handle
        .execute("INSERT INTO blobs VALUES(NULL)")
        .await
        .is_err());
    assert!(handle
        .execute("INSERT INTO items VALUES(CAST(X'ff' AS TEXT),'bad')")
        .await
        .is_err());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn panicked_callback_rolls_back_before_the_next_handle_operation() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let writer = handle.clone();
    let callback = tokio::spawn(async move {
        writer
            .transaction(|tx| async move {
                tx.execute("INSERT INTO items VALUES(1)").await?;
                panic!("native SQLite callback panic");
                #[allow(unreachable_code)]
                Result::<()>::Ok(())
            })
            .await
    });
    assert!(callback.await.unwrap_err().is_panic());
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_and_timed_out_stop_retains_a_submitted_commit_and_client_owner() -> Result<()> {
    let directory = tempfile::TempDir::new()?;
    let mut config = config(SqliteOutput::Changes);
    config.path = Some(directory.path().join("source.db"));
    config.shutdown_timeout_ms = NonZeroU64::new(30).unwrap();
    let client = Arc::new(SqliteClientResource::new(
        ComponentId::try_new("sqlite")?,
        config.clone(),
    )?);
    let mut source = source(config.clone())?.with_client_resource(client.clone())?;
    source.start().await?;
    let handle = client.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let reader = rusqlite::Connection::open(directory.path().join("source.db"))?;
    reader.execute_batch("BEGIN")?;
    reader.query_row("SELECT count(*) FROM items", [], |row| {
        row.get::<_, usize>(0)
    })?;
    let mut write = Box::pin(handle.execute("INSERT INTO items VALUES(1)"));
    assert!(timeout(Duration::from_millis(30), &mut write)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(1), source.stop())
        .await
        .is_err());
    let began = tokio::time::Instant::now();
    let error = source.stop().await.unwrap_err();
    assert!(error.is::<drasi_lib::context::workers::WorkerCleanupError>());
    assert!(began.elapsed() >= Duration::from_millis(30));
    assert!(source.worker.read().await.is_some());
    assert!(source.start().await.is_err());
    assert_eq!(client.owner.available_permits(), 0);
    assert!(SqliteSource::new(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        config.clone()
    )?
    .with_client_resource(client.clone())
    .is_err());
    reader.execute_batch("COMMIT")?;
    assert_eq!(timeout(Duration::from_secs(2), &mut write).await??, 1);
    source.stop().await?;
    assert_eq!(
        reader.query_row("SELECT count(*) FROM items", [], |row| row
            .get::<_, usize>(0))?,
        1
    );
    drop(source);
    assert_eq!(client.owner.available_permits(), 1);
    let mut replacement = SqliteSource::new(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        config,
    )?
    .with_client_resource(client)?;
    replacement.start().await?;
    assert_eq!(handle.query("SELECT * FROM items").await?.len(), 1);
    replacement.stop().await?;
    Ok(())
}
