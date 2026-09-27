// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::interface::{
    CheckpointStore, CreatedIndexes, IndexBackendPlugin, IndexError, SourceCheckpoint,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{DrasiLib, Query, StorageBackendRef};
use drasi_source_postgres::{PostgresReplicationSource, SslMode};
use testcontainers::{core::ContainerPort, runners::AsyncRunner, ImageExt};
use testcontainers_modules::postgres::Postgres;

const SOURCE: &str = "postgres";
const QUERY: &str = "items";
const SLOT: &str = "computation_recovery";
// Persisted input identity "source:postgres", not a framework sequence alone.
const SOURCE_CHECKPOINT: &str = "computation:input:736f757263653a706f737467726573";
const DEADLINE: Duration = Duration::from_secs(30);
const IMAGE: &str =
    "16-alpine@sha256:721873c34ceb9f8d8fc265984940dc982404c105f19ad51be9fdc5970a6080ea";

struct ObservedIndexes {
    inner: RocksDbIndexProvider,
    checkpoints: Mutex<Option<Weak<dyn CheckpointStore>>>,
}

#[async_trait]
impl IndexBackendPlugin for ObservedIndexes {
    async fn create_indexes(&self, id: &str) -> std::result::Result<CreatedIndexes, IndexError> {
        self.create_scoped_indexes(id, id).await
    }
    async fn create_scoped_indexes(
        &self,
        scope: &str,
        id: &str,
    ) -> std::result::Result<CreatedIndexes, IndexError> {
        let indexes = self.inner.create_scoped_indexes(scope, id).await?;
        *self.checkpoints.lock().expect("checkpoint observation") =
            indexes.checkpoint_store.as_ref().map(Arc::downgrade);
        Ok(indexes)
    }
    fn is_volatile(&self) -> bool {
        false
    }
    fn supports_atomic_query_output(&self) -> bool {
        self.inner.supports_atomic_query_output()
    }
}

impl ObservedIndexes {
    async fn checkpoint(&self) -> Result<SourceCheckpoint> {
        let store = self
            .checkpoints
            .lock()
            .expect("checkpoint observation")
            .as_ref()
            .and_then(Weak::upgrade)
            .context("query checkpoint store is unavailable")?;
        store
            .read_checkpoint(SOURCE_CHECKPOINT)
            .await?
            .context("source checkpoint is absent")
    }
}

struct Admin {
    client: tokio_postgres::Client,
    task: tokio::task::JoinHandle<std::result::Result<(), tokio_postgres::Error>>,
}

impl Admin {
    async fn connect(host: &str, port: u16) -> Result<Self> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(host)
            .port(port)
            .user("postgres")
            .password("postgres")
            .dbname("postgres");
        tokio::time::timeout(DEADLINE, async {
            loop {
                match config.connect(tokio_postgres::NoTls).await {
                    Ok((client, connection)) => {
                        let admin = Self {
                            client,
                            task: tokio::spawn(connection),
                        };
                        admin.client.simple_query("SELECT 1").await?;
                        return Ok(admin);
                    }
                    Err(error) => {
                        tracing::debug!("PostgreSQL readiness probe: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        })
        .await?
    }
    async fn close(self, crashed: bool) -> Result<()> {
        drop(self.client);
        let result = tokio::time::timeout(DEADLINE, self.task).await??;
        if crashed {
            anyhow::ensure!(
                result.is_err(),
                "the killed database connection unexpectedly completed cleanly"
            );
        } else {
            result?;
        }
        Ok(())
    }
}

async fn open(root: &Path, host: &str, port: u16) -> Result<(DrasiLib, Arc<ObservedIndexes>)> {
    let indexes = Arc::new(ObservedIndexes {
        inner: RocksDbIndexProvider::new(root.join("indexes"), false, false),
        checkpoints: Mutex::new(None),
    });
    let source = PostgresReplicationSource::builder(SOURCE)
        .with_host(host)
        .with_port(port)
        .with_user("postgres")
        .with_password("postgres")
        .with_database("postgres")
        .with_ssl_mode(SslMode::Disable)
        .with_tables(vec!["items".into()])
        .with_slot_name(SLOT)
        .with_publication_name("items_publication")
        .build()?;
    let core = DrasiLib::builder()
        .with_id("postgres-crash-recovery")
        .with_source(source)
        .with_query(
            Query::cypher(QUERY)
                .query("MATCH (n:items) RETURN n.id AS id, n.name AS name")
                .from_source(SOURCE)
                .enable_bootstrap(false)
                .with_outbox_capacity(32)
                .with_storage_backend(StorageBackendRef::Named("rocks".into()))
                .build(),
        )
        .with_index_provider("rocks", indexes.clone())
        .build()
        .await?;
    core.start().await?;
    tokio::time::timeout(DEADLINE, core.computation_component(QUERY)?.wait_started()).await??;
    Ok((core, indexes))
}

async fn rows(core: &DrasiLib) -> Result<(BTreeMap<i64, String>, u64)> {
    let query = core
        .query_manager()
        .get_query_instance(QUERY)
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    let mut rows = BTreeMap::new();
    for row in snapshot.to_vec() {
        let id = row["id"].as_i64().context("integer item ID")?;
        let name = row["name"].as_str().context("item name")?.to_owned();
        anyhow::ensure!(rows.insert(id, name).is_none(), "duplicate result identity");
    }
    Ok((rows, snapshot.as_of_sequence))
}

async fn wait_rows(core: &DrasiLib, expected: &[(i64, &str)], sequence: u64) -> Result<()> {
    let expected: BTreeMap<_, _> = expected
        .iter()
        .map(|(id, name)| (*id, name.to_string()))
        .collect();
    tokio::time::timeout(DEADLINE, async {
        loop {
            let (actual, head) = rows(core).await?;
            anyhow::ensure!(head <= sequence, "replayed input generated extra output: head={head}, expected={sequence}, rows={actual:?}");
            if actual == expected && head == sequence { return Ok::<_, anyhow::Error>(()); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?
}

fn position_lsn(checkpoint: &SourceCheckpoint) -> Result<u64> {
    let bytes = checkpoint
        .source_position
        .as_ref()
        .context("missing PostgreSQL source position")?;
    anyhow::ensure!(
        bytes.len() == 16,
        "expected commit LSN and transaction offset"
    );
    Ok(u64::from_be_bytes(bytes[..8].try_into()?))
}

async fn assert_feedback_not_ahead(
    client: &tokio_postgres::Client,
    checkpoints: &ObservedIndexes,
) -> Result<()> {
    let confirmed: String = client
        .query_one(
            "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = $1",
            &[&SLOT],
        )
        .await?
        .get(0);
    let (high, low) = confirmed.split_once('/').context("PostgreSQL LSN")?;
    let acknowledged = u64::from_str_radix(high, 16)? << 32 | u64::from_str_radix(low, 16)?;
    let saved = checkpoints.checkpoint().await?;
    anyhow::ensure!(acknowledged <= position_lsn(&saved)?,
        "PostgreSQL retired beyond durable query ownership: acknowledged={confirmed}, checkpoint={saved:?}");
    Ok(())
}

async fn wait_for_live_feedback(
    client: &tokio_postgres::Client,
    checkpoint: &SourceCheckpoint,
    unread: bool,
) -> Result<()> {
    let durable_lsn = position_lsn(checkpoint)?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let row = client
                .query_opt(
                    "SELECT pg_wal_lsn_diff(r.write_lsn, '0/0')::bigint,
                        pg_wal_lsn_diff(s.confirmed_flush_lsn, '0/0')::bigint
                 FROM pg_replication_slots s JOIN pg_stat_replication r ON r.pid=s.active_pid
                 WHERE s.slot_name=$1",
                    &[&SLOT],
                )
                .await?;
            if let Some(row) = row {
                let read: Option<i64> = row.get(0);
                let flushed: i64 = row.get(1);
                let flushed = u64::try_from(flushed)?;
                anyhow::ensure!(
                    flushed <= durable_lsn,
                    "feedback retired unprocessed WAL beyond {checkpoint:?}"
                );
                if flushed == durable_lsn
                    && (!unread || read.is_some_and(|position| position > durable_lsn as i64))
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

async fn data_volume(container: &str) -> Result<String> {
    let inspected = tokio::process::Command::new("docker").args([
        "inspect", "--format",
        "{{range .Mounts}}{{if eq .Destination \"/var/lib/postgresql/data\"}}{{.Name}}{{end}}{{end}}",
        container,
    ]).output().await?;
    anyhow::ensure!(
        inspected.status.success(),
        "inspect owned PostgreSQL volume: {}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let name = String::from_utf8(inspected.stdout)?.trim().to_owned();
    anyhow::ensure!(
        !name.is_empty(),
        "PostgreSQL must use its image-declared Docker volume"
    );
    Ok(name)
}

#[tokio::test]
async fn actual_postgres_crash_keeps_offline_query_changes_and_recovers_exact_rows() -> Result<()> {
    let root = tempfile::tempdir()?;
    // Docker's automatic host-port assignment changes on container restart.
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = reservation.local_addr()?.port();
    let name = format!("computation-postgres-recovery-{}", std::process::id());
    eprintln!("PostgreSQL fixture {name}: {}", root.path().display());
    let image = Postgres::default()
        .with_tag(IMAGE)
        .with_container_name(name)
        .with_mapped_port(port, ContainerPort::Tcp(5432))
        .with_cmd([
            "postgres",
            "-c",
            "wal_level=logical",
            "-c",
            "max_replication_slots=10",
            "-c",
            "max_wal_senders=10",
            "-c",
            "wal_sender_timeout=4s",
            "-c",
            "fsync=on",
            "-c",
            "synchronous_commit=on",
        ]);
    drop(reservation);
    let container = image.start().await?;
    let volume = data_volume(container.id()).await?;
    eprintln!("PostgreSQL retained volume: {volume}");
    let host = container.get_host().await?.to_string();
    let port = container.get_host_port_ipv4(5432).await?;
    let result: Result<()> = async {
        let admin = Admin::connect(&host, port).await?;
        admin
            .client
            .batch_execute(
                "CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT NOT NULL);
             ALTER TABLE items REPLICA IDENTITY FULL;
             CREATE PUBLICATION items_publication FOR TABLE items;",
            )
            .await?;
        let (core, indexes) = open(root.path(), &host, port).await?;
        admin
            .client
            .batch_execute(
                "BEGIN; INSERT INTO items VALUES (1,'alpha'),(2,'beta'),(3,'gamma'); COMMIT;",
            )
            .await?;
        wait_rows(&core, &[(1, "alpha"), (2, "beta"), (3, "gamma")], 3).await?;
        assert_feedback_not_ahead(&admin.client, &indexes).await?;
        wait_for_live_feedback(&admin.client, &indexes.checkpoint().await?, false)
            .await
            .context("wait for actual acknowledgement of the committed prefix")?;
        core.stop_query(QUERY).await?;
        let saved = indexes.checkpoint().await?;
        admin
            .client
            .batch_execute(
                "BEGIN; UPDATE items SET name='alpha-updated' WHERE id=1;
             DELETE FROM items WHERE id=2; INSERT INTO items VALUES (4,'delta'); COMMIT;
             CHECKPOINT;",
            )
            .await?;
        wait_for_live_feedback(&admin.client, &saved, true)
            .await
            .context("source must report newer WAL read without acknowledging paused query work")?;
        assert_eq!(
            indexes.checkpoint().await?,
            saved,
            "stopped query must not acknowledge unseen data"
        );
        let killed = tokio::process::Command::new("docker")
            .args(["kill", "--signal=KILL", container.id()])
            .output()
            .await?;
        anyhow::ensure!(
            killed.status.success(),
            "owned PostgreSQL kill failed: {}",
            String::from_utf8_lossy(&killed.stderr)
        );
        admin
            .close(true)
            .await
            .context("join the killed admin connection")?;
        container
            .start()
            .await
            .context("restart the owned PostgreSQL container")?;
        assert_eq!(
            data_volume(container.id()).await?,
            volume,
            "database restart must retain the original data volume"
        );
        let restarted_port = container.get_host_port_ipv4(5432).await?;
        anyhow::ensure!(
            restarted_port == port,
            "Docker reassigned the PostgreSQL port from {port} to {restarted_port}"
        );
        let admin = Admin::connect(&host, port)
            .await
            .context("PostgreSQL readiness after restart")?;
        let database_rows: BTreeMap<i64, String> = admin
            .client
            .query("SELECT id,name FROM items", &[])
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(
            database_rows,
            BTreeMap::from([
                (1, "alpha-updated".into()),
                (3, "gamma".into()),
                (4, "delta".into())
            ])
        );
        assert_eq!(indexes.checkpoint().await?, saved);
        core.start_query(QUERY)
            .await
            .context("resume query after database restart")?;
        wait_rows(
            &core,
            &[(1, "alpha-updated"), (3, "gamma"), (4, "delta")],
            6,
        )
        .await
        .context("replay offline changes")?;
        assert_feedback_not_ahead(&admin.client, &indexes).await?;
        core.shutdown().await?;
        drop((core, indexes));
        let (reopened, checkpoints) = open(root.path(), &host, port).await?;
        wait_rows(
            &reopened,
            &[(1, "alpha-updated"), (3, "gamma"), (4, "delta")],
            6,
        )
        .await?;
        assert_feedback_not_ahead(&admin.client, &checkpoints).await?;
        reopened.shutdown().await?;
        admin.close(false).await?;
        Ok(())
    }
    .await;
    container.rm().await?;
    result
}
