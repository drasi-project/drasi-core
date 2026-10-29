// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core_types::models::Element;
use futures::{StreamExt, TryStreamExt};
use serde_json::json;
use std::{future::Future, path::Path};
use tokio::time::timeout;

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive test bound")
}

fn millis(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive test timeout")
}

fn settings(path: &Path) -> SqliteBootstrapConfig {
    SqliteBootstrapConfig {
        path: path.to_path_buf(),
        tables: Vec::new(),
        table_keys: Vec::new(),
        max_tables: size(16),
        max_bytes: size(1 << 16),
        max_duration_ms: millis(5000),
        output_capacity: size(1),
        shutdown_timeout_ms: millis(1000),
    }
}

fn provider(config: SqliteBootstrapConfig) -> Result<SqliteBootstrapProvider> {
    SqliteBootstrapProvider::new(
        ComponentId::try_new("source")?,
        StreamId::try_new("source/out")?,
        config,
    )
}

fn seed(sql: &str) -> Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("snapshot.db");
    Connection::open(&path)?.execute_batch(sql)?;
    Ok((directory, path))
}

async fn collect(provider: &SqliteBootstrapProvider) -> Result<Vec<SourceChange>> {
    let snapshot = provider.snapshot().await?;
    assert!(snapshot.watermarks.is_empty());
    let envelopes: Vec<_> =
        timeout(Duration::from_secs(2), snapshot.changes.try_collect()).await??;
    let changes = envelopes
        .iter()
        .map(GraphChangeCodec::decode_changes)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    assert!(provider.complete_snapshot().await?.is_empty());
    assert!(provider.completion_state()?.is_none());
    Ok(changes)
}

fn node(change: &SourceChange) -> &Element {
    let SourceChange::Insert { element } = change else {
        panic!("file snapshots must contain only inserts");
    };
    element
}

fn properties(change: &SourceChange) -> serde_json::Value {
    let Element::Node { properties, .. } = node(change) else {
        panic!("SQLite tables must produce nodes");
    };
    serde_json::Value::Object(properties.into())
}

fn both<F: Future<Output = Result<()>>>(test: impl Fn() -> F) -> Result<()> {
    for threaded in [false, true] {
        let mut runtime = if threaded {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        runtime
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(test())?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn file_snapshots_filter_tables_preserve_types_and_native_typed_keys() -> Result<()> {
    let (_directory, path) = seed(
        "CREATE TABLE items(a,b,real_value REAL,missing,payload BLOB,text_value TEXT,PRIMARY KEY(a,b)) WITHOUT ROWID;
         INSERT INTO items VALUES(X'61','a:b|c',1.5,NULL,X'00FF','hello'),
                                 ('YQ==','a:b|c',2.5,NULL,X'01','other');
         CREATE TABLE ignored(id INTEGER PRIMARY KEY);
         INSERT INTO ignored VALUES(9);
         CREATE TABLE __drasi_private(secret TEXT);
         INSERT INTO __drasi_private VALUES('not application data');",
    )?;
    let mut config = settings(&path);
    config.tables = vec!["items".into()];
    let snapshot = provider(config)?;
    let changes = collect(&snapshot).await?;
    assert_eq!(changes.len(), 2);
    let mut rows: Vec<_> = changes.iter().map(properties).collect();
    rows.sort_by_key(serde_json::Value::to_string);
    let mut expected = vec![
        json!({"a":"YQ==","b":"a:b|c","real_value":1.5,"missing":null,"payload":"AP8=","text_value":"hello"}),
        json!({"a":"YQ==","b":"a:b|c","real_value":2.5,"missing":null,"payload":"AQ==","text_value":"other"}),
    ];
    expected.sort_by_key(serde_json::Value::to_string);
    assert_eq!(rows, expected);
    let identities: Vec<_> = changes
        .iter()
        .map(|change| &node(change).get_metadata().reference)
        .collect();
    assert_ne!(
        identities[0], identities[1],
        "BLOB and TEXT keys cannot collide"
    );
    for change in &changes {
        let metadata = node(change).get_metadata();
        assert_eq!(metadata.reference.source_id.as_ref(), "source");
        assert!(metadata.reference.element_id.starts_with("sqlite:v1:"));
        assert_eq!(&*metadata.labels, [Arc::<str>::from("items")]);
    }
    snapshot.stop().await?;
    let all = provider(settings(&path))?;
    assert_eq!(
        collect(&all).await?.len(),
        3,
        "internal journal tables stay hidden"
    );
    all.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_keys_and_shadowed_physical_rowids_use_the_shared_mapper() -> Result<()> {
    let (_directory, path) = seed(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,code TEXT,value TEXT);
         INSERT INTO items VALUES(1,'custom:key','one');
         CREATE TABLE keyless(rowid TEXT,_rowid_ TEXT,value TEXT);
         INSERT INTO keyless(oid,rowid,_rowid_,value) VALUES(-7,'user-rowid','user-alias','two');
         CREATE TABLE \"quoted\"\"table\"(id INTEGER PRIMARY KEY);
         INSERT INTO \"quoted\"\"table\" VALUES(3);",
    )?;
    let mut config = settings(&path);
    config.table_keys = vec![TableKeyConfig {
        table: "items".into(),
        key_columns: vec!["CODE".into()],
    }];
    let snapshot = provider(config)?;
    let changes = collect(&snapshot).await?;
    assert_eq!(changes.len(), 3);
    let custom = changes
        .iter()
        .find(|change| properties(change)["value"] == "one")
        .context("custom row")?;
    assert_eq!(
        node(custom).get_metadata().reference.element_id.as_ref(),
        "sqlite:v1:[\"items\",{\"code\":[\"text\",\"custom:key\"]}]"
    );
    let keyless = changes
        .iter()
        .find(|change| properties(change)["value"] == "two")
        .context("keyless row")?;
    assert_eq!(
        node(keyless).get_metadata().reference.element_id.as_ref(),
        "sqlite:rowid:v1:[\"keyless\",-7]"
    );
    assert_eq!(
        properties(keyless),
        json!({"rowid":"user-rowid","_rowid_":"user-alias","value":"two"})
    );
    snapshot.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_read_transaction_covers_all_tables_while_wal_writers_commit() -> Result<()> {
    let (_directory, path) = seed(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE first(id INTEGER PRIMARY KEY,value TEXT);
         INSERT INTO first VALUES(1,'before'),(2,'before'),(3,'before');
         CREATE TABLE last(id INTEGER PRIMARY KEY,value TEXT);
         INSERT INTO last VALUES(4,'before');",
    )?;
    let snapshot = provider(settings(&path))?;
    let mut stream = snapshot.snapshot().await?.changes;
    let first = stream.next().await.context("first snapshot row")??;
    let connection = Connection::open(&path)?;
    connection.execute_batch("UPDATE first SET value='after'; UPDATE last SET value='after'")?;
    let mut envelopes = vec![first];
    envelopes.extend(timeout(Duration::from_secs(2), stream.try_collect::<Vec<_>>()).await??);
    assert_eq!(envelopes.len(), 4);
    for envelope in envelopes {
        for change in GraphChangeCodec::decode_changes(&envelope)? {
            assert_eq!(
                properties(&change)["value"],
                "before",
                "one consistent read view, not one per table"
            );
        }
    }
    for change in collect(&snapshot).await? {
        assert_eq!(properties(&change)["value"], "after");
    }
    snapshot.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_files_and_schema_fail_instead_of_returning_empty_snapshots() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing.db");
    for path in [&missing, Path::new(":memory:")] {
        let snapshot = provider(settings(path))?;
        assert!(collect(&snapshot).await.is_err());
        snapshot.stop().await?;
    }
    assert!(
        !missing.exists(),
        "read-only open must not create a database"
    );
    for (sql, table, key, error) in [
        (
            "CREATE TABLE items(id INTEGER)",
            "absent",
            None,
            "missing or unsupported",
        ),
        (
            "CREATE TABLE items(id INTEGER)",
            "items",
            Some("absent"),
            "key column",
        ),
        (
            "CREATE TABLE items(id INTEGER);INSERT INTO items VALUES(NULL)",
            "items",
            Some("id"),
            "cannot be null",
        ),
        (
            "CREATE TABLE items(rowid,_rowid_,oid)",
            "items",
            None,
            "shadows every rowid alias",
        ),
        (
            "CREATE TABLE items(id INTEGER,value GENERATED ALWAYS AS (id+1))",
            "items",
            None,
            "generated/hidden",
        ),
    ] {
        let (_directory, path) = seed(sql)?;
        let mut config = settings(&path);
        config.tables = vec![table.into()];
        if let Some(key) = key {
            config.table_keys = vec![TableKeyConfig {
                table: table.into(),
                key_columns: vec![key.into()],
            }];
        }
        let snapshot = provider(config)?;
        let failure = collect(&snapshot)
            .await
            .expect_err("invalid schema must fail");
        assert!(format!("{failure:#}").contains(error), "{failure:#}");
        snapshot.stop().await?;
    }
    let (_directory, path) = seed("CREATE TABLE items(id INTEGER PRIMARY KEY)")?;
    let empty = provider(settings(&path))?;
    assert!(
        collect(&empty).await?.is_empty(),
        "a real empty file snapshot is valid"
    );
    empty.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn schema_and_exact_row_byte_limits_reject_instead_of_truncating() -> Result<()> {
    let (_directory, path) = seed(
        "CREATE TABLE first(id INTEGER PRIMARY KEY,value TEXT);
         INSERT INTO first VALUES(1,'a payload longer than a tiny record');
         CREATE TABLE second(id INTEGER PRIMARY KEY);",
    )?;
    let mut config = settings(&path);
    config.max_tables = size(1);
    let snapshot = provider(config.clone())?;
    assert!(format!(
        "{:#}",
        collect(&snapshot)
            .await
            .expect_err("two tables exceed the bound")
    )
    .contains("table count"));
    snapshot.stop().await?;
    config.tables = vec!["first".into()];
    let snapshot = provider(config.clone())?;
    let changes = collect(&snapshot).await?;
    let bytes = usize::try_from(bincode::serialized_size(&changes[0])?)?;
    snapshot.stop().await?;
    config.max_bytes = size(bytes);
    let exact = provider(config.clone())?;
    assert_eq!(
        collect(&exact).await?.len(),
        1,
        "the precise encoded size is allowed"
    );
    exact.stop().await?;
    config.max_bytes = size(bytes - 1);
    let small = provider(config)?;
    assert!(format!(
        "{:#}",
        collect(&small)
            .await
            .expect_err("one byte below the record size")
    )
    .contains("row exceeds byte limit"));
    small.stop().await?;
    Ok(())
}

#[test]
fn configuration_rejects_invalid_identifiers_and_unrepresentable_bounds() -> Result<()> {
    let config = settings(Path::new("file.db"));
    for case in [
        "path",
        "reserved",
        "duplicate",
        "keys",
        "capacity",
        "bytes",
        "duration",
    ] {
        let mut invalid = config.clone();
        match case {
            "path" => invalid.path = PathBuf::new(),
            "reserved" => invalid.tables = vec!["__DRASI_private".into()],
            "duplicate" => invalid.tables = vec!["items".into(), "items".into()],
            "keys" => {
                invalid.table_keys = vec![TableKeyConfig {
                    table: "items".into(),
                    key_columns: vec!["id".into(), "ID".into()],
                }]
            }
            "capacity" => invalid.output_capacity = size(tokio::sync::Semaphore::MAX_PERMITS + 1),
            "bytes" => invalid.max_bytes = size(i32::MAX as usize + 1),
            "duration" => invalid.max_duration_ms = millis(i32::MAX as u64 + 1),
            _ => unreachable!(),
        }
        assert!(provider(invalid).is_err(), "{case}");
    }
    let mut long_cleanup = config.clone();
    long_cleanup.shutdown_timeout_ms = millis(u64::MAX);
    let representable = Instant::now().checked_add(long_cleanup.timeout()).is_some();
    assert_eq!(provider(long_cleanup).is_ok(), representable);
    let mut value = serde_json::to_value(&config)?;
    value["output_capacity"] = json!(0);
    assert!(serde_json::from_value::<SqliteBootstrapConfig>(value).is_err());
    Ok(())
}

#[test]
fn stopped_and_dropped_snapshot_readers_cannot_touch_replacement_workers() -> Result<()> {
    both(|| async {
        let (_directory, path) = seed(
            "CREATE TABLE items(id INTEGER PRIMARY KEY);
             INSERT INTO items VALUES(1),(2),(3),(4);",
        )?;
        let snapshot = provider(settings(&path))?;
        let mut old = snapshot.snapshot().await?.changes;
        old.next().await.context("old first row")??;
        assert!(
            snapshot.snapshot().await.is_err(),
            "even finished workers need their join observed"
        );
        snapshot.stop().await?;
        let replacement = snapshot.snapshot().await?;
        assert!(format!(
            "{:#}",
            old.next()
                .await
                .context("stale stream error")?
                .expect_err("retired reader")
        )
        .contains("retired"));
        drop(old);
        let rows = timeout(
            Duration::from_secs(2),
            replacement.changes.try_collect::<Vec<_>>(),
        )
        .await??;
        assert_eq!(
            rows.len(),
            4,
            "old reader drop cannot cancel the replacement"
        );
        let dropped = snapshot.snapshot().await?;
        drop(dropped);
        assert!(
            snapshot.snapshot().await.is_err(),
            "drop requests cancellation, not completed cleanup"
        );
        snapshot.stop().await?;
        assert_eq!(collect(&snapshot).await?.len(), 4);
        snapshot.stop().await?;
        Ok(())
    })
}

#[test]
fn cancelled_and_timed_out_cleanup_keep_the_actual_blocking_owner() -> Result<()> {
    both(|| async {
        let (_directory, path) =
            seed("CREATE TABLE items(id INTEGER PRIMARY KEY);INSERT INTO items VALUES(1)")?;
        let mut config = settings(&path);
        config.shutdown_timeout_ms = millis(10);
        let snapshot = provider(config)?;
        let run = Arc::new(SnapshotRun {
            shutdown: watch::channel(false).0,
        });
        *snapshot.shared.run.lock().expect("run lock") = Some(run.clone());
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, hold) = std::sync::mpsc::channel();
        spawn_owned_blocking_worker(&snapshot.shared.worker, move || {
            entered
                .send(())
                .map_err(|_| anyhow::anyhow!("test lost entry receiver"))?;
            hold.recv()?;
            Ok(false)
        })
        .await?;
        ready.await?;
        {
            let stop = snapshot.stop();
            tokio::pin!(stop);
            assert!(futures::poll!(&mut stop).is_pending());
        }
        assert!(*run.shutdown.borrow());
        assert!(snapshot.shared.worker.read().await.is_some());
        assert!(snapshot.snapshot().await.is_err());
        assert!(
            snapshot.stop().await.is_err(),
            "timeout must retain ownership"
        );
        assert!(snapshot.shared.worker.read().await.is_some());
        assert!(snapshot.snapshot().await.is_err());
        release.send(())?;
        snapshot.stop().await?;
        assert!(snapshot.shared.worker.read().await.is_none());
        assert_eq!(collect(&snapshot).await?.len(), 1);
        snapshot.stop().await?;
        Ok(())
    })
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_backpressure_has_an_explicit_deadline_failure() -> Result<()> {
    let (_directory, path) =
        seed("CREATE TABLE items(id INTEGER PRIMARY KEY);INSERT INTO items VALUES(1),(2),(3),(4)")?;
    let mut config = settings(&path);
    config.max_duration_ms = millis(40);
    let snapshot = provider(config)?;
    let stream = snapshot.snapshot().await?;
    timeout(Duration::from_secs(1), async {
        loop {
            if snapshot
                .shared
                .worker
                .read()
                .await
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let error = stream
        .changes
        .try_collect::<Vec<_>>()
        .await
        .expect_err("blocked output must expire");
    assert!(format!("{error:#}").contains("deadline"), "{error:#}");
    snapshot.stop().await?;
    Ok(())
}
