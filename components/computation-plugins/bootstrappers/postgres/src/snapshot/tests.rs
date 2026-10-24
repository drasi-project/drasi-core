// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{
    SourceProgressProvider, SourceProgressSnapshot, SourceProgressUpdates,
};
use drasi_core_types::interface::StorageDurability;
use futures::TryStreamExt;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct Progress(ComponentId);
impl SourceProgressProvider for Progress {
    fn graph_id(&self) -> &str {
        "graph"
    }
    fn component_id(&self) -> &ComponentId {
        &self.0
    }
    fn snapshot(&self) -> Result<Arc<SourceProgressSnapshot>> {
        Ok(Arc::new(SourceProgressSnapshot {
            recovered: true,
            persistent: true,
            ..Default::default()
        }))
    }
    fn subscribe(&self) -> Result<Box<dyn SourceProgressUpdates>> {
        anyhow::bail!("snapshot initialization must not subscribe to live progress")
    }
}

#[derive(Default)]
struct State(Mutex<Option<Bytes>>);
#[async_trait]
impl BootstrapState for State {
    fn durability(&self) -> StorageDurability {
        StorageDurability::LOCAL_PROCESS_RESTART
    }
    async fn read(&self) -> Result<Option<Bytes>> {
        Ok(self.0.lock().unwrap().clone())
    }
    async fn write(&self, state: Bytes) -> Result<()> {
        *self.0.lock().unwrap() = Some(state);
        Ok(())
    }
}

async fn frame(stream: &mut TcpStream, tag: u8, bytes: &[u8]) -> Result<()> {
    stream.write_u8(tag).await?;
    stream.write_u32(u32::try_from(bytes.len() + 4)?).await?;
    stream.write_all(bytes).await?;
    Ok(())
}

async fn handshake(stream: &mut TcpStream) -> Result<()> {
    let length = stream.read_u32().await?;
    anyhow::ensure!((8..65536).contains(&length), "invalid startup frame");
    stream.read_exact(&mut vec![0; length as usize - 4]).await?;
    frame(stream, b'R', &[0, 0, 0, 0]).await?;
    frame(stream, b'Z', b"I").await
}

async fn query(stream: &mut TcpStream) -> Result<String> {
    anyhow::ensure!(stream.read_u8().await? == b'Q', "expected simple query");
    let length = stream.read_u32().await?;
    anyhow::ensure!((5..65536).contains(&length), "invalid query frame");
    let mut bytes = vec![0; length as usize - 4];
    stream.read_exact(&mut bytes).await?;
    anyhow::ensure!(bytes.pop() == Some(0), "query is not terminated");
    Ok(String::from_utf8(bytes)?)
}

async fn response(stream: &mut TcpStream, rows: &[&[Option<&str>]]) -> Result<()> {
    if let Some(row) = rows.first() {
        let mut description = u16::try_from(row.len())?.to_be_bytes().to_vec();
        for column in 0..row.len() {
            description.extend(format!("column{column}\0").as_bytes());
            description.extend(0u32.to_be_bytes());
            description.extend(0u16.to_be_bytes());
            description.extend(25u32.to_be_bytes());
            description.extend((-1i16).to_be_bytes());
            description.extend((-1i32).to_be_bytes());
            description.extend(0u16.to_be_bytes());
        }
        frame(stream, b'T', &description).await?;
    }
    for row in rows {
        let mut data = u16::try_from(row.len())?.to_be_bytes().to_vec();
        for value in *row {
            match value {
                Some(value) => {
                    data.extend(i32::try_from(value.len())?.to_be_bytes());
                    data.extend(value.as_bytes());
                }
                None => data.extend((-1i32).to_be_bytes()),
            }
        }
        frame(stream, b'D', &data).await?;
    }
    frame(stream, b'C', b"SELECT 1\0").await?;
    frame(stream, b'Z', b"I").await
}

async fn database(listener: TcpListener, state: &State) -> Result<()> {
    let (mut exporter, _) = listener.accept().await?;
    handshake(&mut exporter).await?;
    let mut clock_read = false;
    loop {
        let sql = query(&mut exporter).await?;
        if sql == "IDENTIFY_SYSTEM" {
            response(
                &mut exporter,
                &[&[Some("123"), Some("1"), Some("0/10"), Some("database")]],
            )
            .await?;
        } else if sql == drasi_postgres_common::PRIMARY_KEY_QUERY {
            response(
                &mut exporter,
                &[&[Some("public"), Some("person"), Some("id")]],
            )
            .await?;
        } else if sql.starts_with("SELECT schemaname, tablename FROM") {
            response(&mut exporter, &[&[Some("public"), Some("person")]]).await?;
        } else if sql.starts_with("SELECT pubinsert, pubupdate") {
            response(
                &mut exporter,
                &[&[
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("f"),
                    Some("f"),
                ]],
            )
            .await?;
        } else if sql == "SHOW server_version_num" {
            response(&mut exporter, &[&[Some("160000")]]).await?;
        } else if sql.starts_with("SELECT schemaname, tablename, attnames") {
            response(
                &mut exporter,
                &[&[Some("public"), Some("person"), Some("{id,name}"), None]],
            )
            .await?;
        } else if sql.starts_with("SELECT p.schemaname, p.tablename, a.attname") {
            response(
                &mut exporter,
                &[
                    &[
                        Some("public"),
                        Some("person"),
                        Some("id"),
                        Some("23"),
                        Some("-1"),
                        Some("d"),
                    ],
                    &[
                        Some("public"),
                        Some("person"),
                        Some("name"),
                        Some("25"),
                        Some("-1"),
                        Some("d"),
                    ],
                ],
            )
            .await?;
        } else if sql.starts_with("SELECT current_setting('server_version_num')") {
            response(
                &mut exporter,
                &[&[Some("t"), Some("t"), Some("t"), Some("t"), Some("t")]],
            )
            .await?;
        } else if sql.starts_with("SELECT 1 FROM pg_publication_tables") {
            response(&mut exporter, &[]).await?;
        } else if sql
            == "SELECT floor(extract(epoch FROM pg_catalog.clock_timestamp()) * 1000)::bigint"
        {
            clock_read = true;
            response(&mut exporter, &[&[Some("123")]]).await?;
        } else if sql.starts_with("CREATE_REPLICATION_SLOT ") {
            anyhow::ensure!(
                clock_read,
                "snapshot must sample PostgreSQL time before its boundary"
            );
            let slot = sql.split_whitespace().nth(1).context("slot name")?;
            let intent: Handover = serde_json::from_slice(
                state
                    .0
                    .lock()
                    .unwrap()
                    .as_deref()
                    .context("intent must precede slot creation")?,
            )?;
            assert_eq!(intent.slot, slot);
            assert!(!intent.completed && intent.boundary.is_none());
            response(
                &mut exporter,
                &[&[Some(slot), Some("0/10"), Some("A-1"), Some("pgoutput")]],
            )
            .await?;
            break;
        } else {
            anyhow::bail!("unexpected exporter query: {sql}");
        }
    }
    let (mut reader, _) = listener.accept().await?;
    handshake(&mut reader).await?;
    assert_eq!(
        query(&mut reader).await?,
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET TRANSACTION SNAPSHOT 'A-1'"
    );
    response(&mut reader, &[]).await?;
    drop(exporter);
    let mut fetched = false;
    loop {
        let sql = query(&mut reader).await?;
        if sql.starts_with("SELECT current_setting('max_slot_wal_keep_size'") {
            response(&mut reader, &[&[Some("-1"), None, Some("f")]]).await?;
        } else if sql.starts_with("LOCK TABLE ") || sql.starts_with("DECLARE drasi_snapshot ") {
            response(&mut reader, &[]).await?;
        } else if sql.starts_with("SELECT a.attname, a.atttypid::text") {
            response(
                &mut reader,
                &[&[Some("id"), Some("23")], &[Some("name"), Some("25")]],
            )
            .await?;
        } else if sql == "FETCH FORWARD 1 FROM drasi_snapshot" {
            if fetched {
                response(&mut reader, &[]).await?;
            } else {
                fetched = true;
                response(&mut reader, &[&[Some("1"), Some("initial")]]).await?;
            }
        } else if sql == "CLOSE drasi_snapshot" {
            response(&mut reader, &[]).await?;
        } else if sql == "COMMIT" {
            response(&mut reader, &[]).await?;
            return Ok(());
        } else {
            anyhow::bail!("unexpected snapshot reader query: {sql}");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_rows_use_database_clock_before_boundary_not_host_wall_clock() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let config: PostgresTransactionConfig = serde_json::from_value(serde_json::json!({
        "connection": {
            "host": "127.0.0.1", "port": listener.local_addr()?.port(),
            "database": "database", "user": "user", "ssl_mode": "disable",
            "slot_name": "owned", "publication_name": "publication",
        },
        "start_lsn": "0/0",
        "transactions": {"max_changes": 16, "max_bytes": 65536, "max_duration_ms": 5000},
        "max_protocol_bytes": 65536, "io_timeout_ms": 5000, "feedback_interval_ms": 100
    }))?;
    let provider = PostgresSnapshot::new(
        ComponentId::try_new("source")?,
        StreamId::try_new("source/out")?,
        config,
        SourceProgressView::new(Arc::new(Progress(ComponentId::try_new("query")?))),
    )?;
    let state = State::default();
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(database(listener, &state), async {
            provider.prepare_with_state(&state).await?;
            let snapshot = provider.snapshot_with_state(&state).await?;
            let envelopes = snapshot.changes.try_collect::<Vec<_>>().await?;
            assert_eq!(envelopes.len(), 1);
            let changes = GraphChangeCodec::decode_changes(&envelopes[0])?;
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].get_transaction_time(), 123);
            let watermarks = provider.complete_snapshot().await?;
            assert_eq!(watermarks[0].sequence, 16, "the LSN is not the timestamp");
            let completed: Handover = serde_json::from_slice(
                &provider.completion_state()?.context("completed handover")?,
            )?;
            assert!(completed.completed);
            let saved: Handover = serde_json::from_slice(
                state.0.lock().unwrap().as_deref().context("saved intent")?,
            )?;
            assert!(
                !saved.completed,
                "only the host may commit completed handover"
            );
            provider.stop().await?;
            Result::<()>::Ok(())
        })?;
        Result::<()>::Ok(())
    })
    .await??;
    Ok(())
}
