// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    version: u32,
    epoch: Uuid,
    sequence: u64,
}

pub(super) fn identity(epoch: Uuid, sequence: u64) -> Result<Bytes> {
    Ok(Bytes::from(serde_json::to_vec(&Position {
        version: 1,
        epoch,
        sequence,
    })?))
}

pub(super) fn binding(
    config: &SqliteConfig,
    source: &ComponentId,
    stream: &StreamId,
    owner: &SourceProgressReader,
    coordinated: bool,
) -> Result<String> {
    let mut tables = config.tables.clone();
    tables.sort();
    let binding = serde_json::to_string(&(
        1u32,
        source.as_str(),
        stream.as_str(),
        owner.graph_id(),
        owner.component_id().as_str(),
        tables,
    ))?;
    Ok(if coordinated {
        serde_json::to_string(&("coordinated-v1", binding))?
    } else {
        binding
    })
}

pub(super) fn checkpoint(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
    epoch: Uuid,
) -> Result<Option<u64>> {
    let Some(checkpoint) = snapshot
        .checkpoints
        .get(&SourceProgressKey::Source(source.to_string()))
    else {
        return Ok(None);
    };
    let position: Position = serde_json::from_slice(
        checkpoint
            .source_position
            .as_deref()
            .context("native SQLite cursor has no position")?,
    )?;
    anyhow::ensure!(
        position.version == 1
            && position.epoch == epoch
            && position.sequence == checkpoint.sequence,
        "native SQLite cursor identity does not match the retained journal"
    );
    Ok(Some(position.sequence))
}

pub(super) struct Journal {
    pub source: ComponentId,
    pub epoch: Uuid,
    pub head: u64,
    pub retired: u64,
    pub bytes: u64,
    pub sent: u64,
    pub schema: String,
    reset_generation: u64,
    limits: SqliteReplayConfig,
}

impl Journal {
    pub fn open(
        connection: &Connection,
        config: &SqliteConfig,
        source: ComponentId,
        stream: &StreamId,
        progress: (&SourceProgressReader, &SourceProgressSnapshot),
        schema: String,
        bootstrap: Option<Uuid>,
    ) -> Result<Self> {
        let (owner, snapshot) = progress;
        let limits = config
            .replay
            .clone()
            .context("SQLite journal settings missing")?;
        let binding = binding(config, &source, stream, owner, bootstrap.is_some())?;
        let existing: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='__drasi_native_state_v1')",
            [],
            |row| row.get(0),
        )?;
        let transaction = connection.unchecked_transaction()?;
        if !existing {
            anyhow::ensure!(
                !snapshot
                    .checkpoints
                    .contains_key(&SourceProgressKey::Source(source.to_string())),
                "native SQLite journal is missing for an existing consumer cursor"
            );
            let mut listing = transaction.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND type='table' AND lower(substr(name,1,7)) != 'sqlite_'")?;
            let mut rows = listing.query([])?;
            while let Some(row) = rows.next()? {
                let table: String = row.get(0)?;
                anyhow::ensure!(
                    !table.to_ascii_lowercase().starts_with("__drasi_"),
                    "unrecognized native SQLite journal; explicit recovery required"
                );
                if !config.tables.is_empty() && !config.tables.contains(&table) {
                    continue;
                }
                let populated: bool = transaction.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM \"{}\")",
                        table.replace('"', "\"\"")
                    ),
                    [],
                    |row| row.get(0),
                )?;
                anyhow::ensure!(
                    !populated || bootstrap.is_some(),
                    "native SQLite replay cannot adopt existing rows without coordinated bootstrap"
                );
            }
            drop(rows);
            drop(listing);
            transaction.execute_batch(
                "CREATE TABLE __drasi_native_state_v1(
                   singleton INTEGER PRIMARY KEY CHECK(singleton=1), binding TEXT NOT NULL,
                   epoch TEXT NOT NULL, head INTEGER NOT NULL, retired INTEGER NOT NULL, bytes INTEGER NOT NULL,
                   source_schema TEXT NOT NULL);
                 CREATE TABLE __drasi_native_outbox_v1(
                   sequence INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, payload BLOB NOT NULL);"
            )?;
            transaction.execute(
                "INSERT INTO __drasi_native_state_v1 VALUES(1,?1,?2,0,0,0,?3)",
                params![
                    binding,
                    bootstrap.unwrap_or_else(Uuid::new_v4).to_string(),
                    schema
                ],
            )?;
            if let Some(epoch) = bootstrap {
                transaction.execute_batch("CREATE TABLE __drasi_native_bootstrap_v1(singleton INTEGER PRIMARY KEY CHECK(singleton=1), epoch TEXT NOT NULL)")?;
                transaction.execute(
                    "INSERT INTO __drasi_native_bootstrap_v1 VALUES(1,?1)",
                    [epoch.to_string()],
                )?;
            }
        }
        let coordinated: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='__drasi_native_bootstrap_v1')",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            coordinated == bootstrap.is_some(),
            "native SQLite coordinated journal requires its paired snapshot; initialization mode cannot change"
        );
        if let Some(expected) = bootstrap {
            let epoch: String = transaction.query_row(
                "SELECT epoch FROM __drasi_native_bootstrap_v1 WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                Uuid::parse_str(&epoch)? == expected,
                "native SQLite bootstrap journal identity changed"
            );
        }
        let (stored, epoch, head, retired, bytes): (String, String, u64, u64, u64) = transaction.query_row(
            "SELECT binding,epoch,head,retired,bytes FROM __drasi_native_state_v1 WHERE singleton=1", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        anyhow::ensure!(stored == binding, "native SQLite journal belongs to a different source, stream, consumer or table selection");
        let recorded_schema: String = transaction.query_row(
            "SELECT source_schema FROM __drasi_native_state_v1 WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            schema == recorded_schema,
            "native SQLite schema changed outside its transaction owner"
        );
        anyhow::ensure!(retired <= head, "native SQLite journal progress is damaged");
        let (count, total, low, high): (u64, u64, Option<u64>, Option<u64>) = transaction.query_row(
            "SELECT count(*),coalesce(sum(length(payload)),0),min(sequence),max(sequence) FROM __drasi_native_outbox_v1", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        anyhow::ensure!(
            count == head - retired
                && total == bytes
                && (count == 0 || (low == Some(retired + 1) && high == Some(head))),
            "native SQLite journal has missing or damaged records"
        );
        anyhow::ensure!(
            count <= limits.max_transactions.get() as u64 && bytes <= limits.max_bytes.get() as u64,
            "native SQLite retained journal exceeds configured limits"
        );
        let epoch = Uuid::parse_str(&epoch)?;
        anyhow::ensure!(
            bootstrap.is_none_or(|expected| expected == epoch),
            "native SQLite bootstrap and journal identities disagree"
        );
        let mut journal = Self {
            source,
            epoch,
            head,
            retired,
            bytes,
            sent: head,
            reset_generation: snapshot.reset_generation,
            limits,
            schema,
        };
        let resume = journal.position(snapshot)?;
        anyhow::ensure!(
            resume >= retired && resume <= head,
            "native SQLite consumer cursor is outside retained history"
        );
        journal.sent = resume;
        transaction.commit()?;
        Ok(journal)
    }

    pub fn position(&self, snapshot: &SourceProgressSnapshot) -> Result<u64> {
        anyhow::ensure!(
            snapshot.failure.is_none()
                && snapshot.persistent
                && snapshot.reset_generation == self.reset_generation,
            "native SQLite consumer progress is fenced or reset"
        );
        let Some(position) = checkpoint(snapshot, &self.source, self.epoch)? else {
            anyhow::ensure!(
                self.retired == 0,
                "native SQLite consumer has no cursor for already retired history"
            );
            return Ok(0);
        };
        Ok(position)
    }

    pub fn identity(&self, sequence: u64) -> Result<Bytes> {
        identity(self.epoch, sequence)
    }

    pub fn append(
        &self,
        connection: &Connection,
        payload: &[u8],
        timestamp: DateTime<Utc>,
    ) -> Result<(u64, u64)> {
        let head = self
            .head
            .checked_add(1)
            .filter(|head| *head <= i64::MAX as u64)
            .context("native SQLite journal sequence exhausted")?;
        let bytes = self
            .bytes
            .checked_add(payload.len() as u64)
            .context("native SQLite journal byte count overflow")?;
        anyhow::ensure!(
            head - self.retired <= self.limits.max_transactions.get() as u64
                && bytes <= self.limits.max_bytes.get() as u64,
            "native SQLite journal is full; transaction was not committed"
        );
        connection.execute(
            "INSERT INTO __drasi_native_outbox_v1 VALUES(?1,?2,?3)",
            params![head, timestamp.timestamp_millis(), payload],
        )?;
        anyhow::ensure!(connection.execute("UPDATE __drasi_native_state_v1 SET head=?1,bytes=?2 WHERE singleton=1 AND head=?3 AND bytes=?4",
            params![head, bytes, self.head, self.bytes])? == 1, "native SQLite journal head changed unexpectedly");
        Ok((head, bytes))
    }

    pub fn feedback(
        &mut self,
        connection: &Connection,
        snapshot: &SourceProgressSnapshot,
    ) -> Result<()> {
        let position = self.position(snapshot)?;
        anyhow::ensure!(
            position >= self.retired && position <= self.sent,
            "native SQLite consumer progress moved outside emitted history"
        );
        if !snapshot.ready || position == self.retired {
            return Ok(());
        }
        let transaction = connection.unchecked_transaction()?;
        let removed: u64 = transaction.query_row(
            "SELECT coalesce(sum(length(payload)),0) FROM __drasi_native_outbox_v1 WHERE sequence<=?1", [position], |row| row.get(0),
        )?;
        let bytes = self
            .bytes
            .checked_sub(removed)
            .context("native SQLite journal byte accounting damaged")?;
        let deleted = transaction.execute(
            "DELETE FROM __drasi_native_outbox_v1 WHERE sequence<=?1",
            [position],
        )?;
        anyhow::ensure!(
            deleted as u64 == position - self.retired,
            "native SQLite retirement has missing records"
        );
        anyhow::ensure!(transaction.execute("UPDATE __drasi_native_state_v1 SET retired=?1,bytes=?2 WHERE singleton=1 AND retired=?3 AND bytes=?4",
            params![position, bytes, self.retired, self.bytes])? == 1, "native SQLite retirement changed unexpectedly");
        transaction.commit()?;
        self.retired = position;
        self.bytes = bytes;
        Ok(())
    }

    pub fn next(
        &mut self,
        connection: &Connection,
        limits: SourceTransactionLimits,
    ) -> Result<Option<(u64, DateTime<Utc>, SourceTransaction)>> {
        let row: Option<(u64, i64, Vec<u8>)> = connection.query_row(
            "SELECT sequence,timestamp,payload FROM __drasi_native_outbox_v1 WHERE sequence>?1 ORDER BY sequence LIMIT 1",
            [self.sent], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let Some((sequence, timestamp, payload)) = row else {
            anyhow::ensure!(
                self.sent == self.head,
                "native SQLite replay record is missing"
            );
            return Ok(None);
        };
        anyhow::ensure!(
            sequence == self.sent + 1,
            "native SQLite replay sequence has a gap"
        );
        let transaction = SourceTransactionCodec::decode_committed(&payload, limits)?;
        let identity = self.identity(sequence)?;
        anyhow::ensure!(
            transaction.source_id() == self.source.as_str()
                && transaction.transaction_id() == identity.as_ref()
                && transaction.position() == identity.as_ref(),
            "native SQLite replay record identity is damaged"
        );
        let timestamp = DateTime::from_timestamp_millis(timestamp)
            .context("invalid native SQLite journal timestamp")?;
        self.sent = sequence;
        Ok(Some((sequence, timestamp, transaction)))
    }
}
