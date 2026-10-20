// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    progress::{Generations, Observed, Reader},
    SqliteConfig, SqliteReplayConfig,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_computation_contracts::{
    ComponentId, ConsumerProgressView, InputPositionComparator, InputPositionPolicy,
    SourceProgressKey, SourceProgressSnapshot, SourceProgressView, SourceTransaction,
    SourceTransactionCodec, SourceTransactionLimits, StreamId, MAX_CONSUMER_PROGRESS_MEMBERS,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Handover {
    pub version: u32,
    pub epoch: Uuid,
    pub binding: String,
    pub completed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupHandover {
    pub version: u32,
    pub binding: String,
    pub consumer: ComponentId,
    pub epoch: Uuid,
    pub sequence: u64,
    pub completed: bool,
}

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
    owner: &SourceProgressView,
    coordinated: bool,
) -> Result<String> {
    let mut tables = config.tables.clone();
    tables.sort();
    let mut binding = serde_json::to_string(&(
        1u32,
        source.as_str(),
        stream.as_str(),
        owner.graph_id(),
        owner.component_id().as_str(),
        tables,
    ))?;
    if !config.table_keys.is_empty() {
        let mut keys = config.table_keys.clone();
        keys.sort_by(|left, right| left.table.cmp(&right.table));
        binding = serde_json::to_string(&("keys-v1", binding, keys))?;
    }
    Ok(if coordinated {
        serde_json::to_string(&("coordinated-v1", binding))?
    } else {
        binding
    })
}

pub(super) fn group_binding(
    config: &SqliteConfig,
    source: &ComponentId,
    stream: &StreamId,
    owners: &ConsumerProgressView,
    coordinated: bool,
) -> Result<String> {
    let mut tables = config.tables.clone();
    tables.sort();
    let mut binding = serde_json::to_string(&(
        "consumer-group-v1",
        source.as_str(),
        stream.as_str(),
        owners.graph_id(),
        owners.producer_id().as_str(),
        tables,
    ))?;
    if !config.table_keys.is_empty() {
        let mut keys = config.table_keys.clone();
        keys.sort_by(|left, right| left.table.cmp(&right.table));
        binding = serde_json::to_string(&("keys-v1", binding, keys))?;
    }
    Ok(if coordinated {
        serde_json::to_string(&("coordinated-v1", binding))?
    } else {
        binding
    })
}

struct CursorOrder;
impl InputPositionComparator for CursorOrder {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> Result<bool> {
        let event: Position = serde_json::from_slice(event)?;
        let handled: Position = serde_json::from_slice(handled)?;
        anyhow::ensure!(
            event.version == 1 && handled.version == 1 && event.epoch == handled.epoch,
            "native SQLite cursor comparison requires the same journal identity"
        );
        Ok(event.sequence > handled.sequence)
    }
}

pub(super) fn position_policy(source: &ComponentId) -> Result<Arc<InputPositionPolicy>> {
    Ok(Arc::new(InputPositionPolicy::new(
        SourceProgressKey::Source(source.to_string()),
        Arc::new(CursorOrder),
    )?))
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

pub(super) fn plan_group_snapshot(
    connection: &Connection,
    binding: &str,
    consumer: &ComponentId,
    previous: Option<&GroupHandover>,
) -> Result<GroupHandover> {
    if let Some(previous) = previous {
        anyhow::ensure!(
            previous.version == 1
                && previous.binding == binding
                && &previous.consumer == consumer
                && !previous.completed,
            "native SQLite shared snapshot identity changed or initialization is already complete"
        );
    }
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='__drasi_native_state_v1')",
        [],
        |row| row.get(0),
    )?;
    let (epoch, sequence) = if exists {
        let (stored, epoch, head): (String, String, u64) = connection.query_row(
            "SELECT binding,epoch,head FROM __drasi_native_state_v1 WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        anyhow::ensure!(
            stored == binding,
            "native SQLite journal belongs to another binding"
        );
        let epoch = Uuid::parse_str(&epoch)?;
        let member: Option<(u64, bool)> = connection
            .query_row(
                "SELECT boundary,confirmed FROM __drasi_native_consumers_v1 WHERE consumer=?1",
                [consumer.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match (previous, member) {
            (Some(previous), Some((boundary, false))) => {
                anyhow::ensure!(
                    previous.epoch == epoch && previous.sequence == boundary && boundary == head,
                    "native SQLite incomplete snapshot boundary changed"
                );
            }
            (Some(previous), None) => {
                anyhow::ensure!(
                    previous.epoch == epoch && previous.sequence == head,
                    "native SQLite initial snapshot intent no longer matches the journal"
                );
            }
            (None, None) => {}
            _ => anyhow::bail!(
                "native SQLite consumer {consumer} lost its recorded snapshot ownership"
            ),
        }
        (epoch, head)
    } else {
        anyhow::ensure!(
            previous.is_none_or(|previous| previous.sequence == 0),
            "native SQLite shared snapshot journal is missing"
        );
        (
            previous.map_or_else(Uuid::new_v4, |previous| previous.epoch),
            0,
        )
    };
    Ok(GroupHandover {
        version: 1,
        binding: binding.into(),
        consumer: consumer.clone(),
        epoch,
        sequence,
        completed: false,
    })
}

pub(super) fn enroll_group_snapshot(
    connection: &Connection,
    handover: &GroupHandover,
) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    let existing: Option<(u64, bool)> = transaction
        .query_row(
            "SELECT boundary,confirmed FROM __drasi_native_consumers_v1 WHERE consumer=?1",
            [handover.consumer.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((boundary, confirmed)) = existing {
        anyhow::ensure!(
            !confirmed && boundary == handover.sequence,
            "native SQLite snapshot enrollment changed"
        );
    } else {
        let inserted = transaction.execute(
            "INSERT INTO __drasi_native_consumers_v1
             SELECT ?1,?2,?2,0,1 WHERE
             (SELECT count(*) FROM __drasi_native_consumers_v1) < ?3",
            params![
                handover.consumer.as_str(),
                handover.sequence,
                MAX_CONSUMER_PROGRESS_MEMBERS as u64
            ],
        )?;
        anyhow::ensure!(
            inserted == 1,
            "native SQLite recorded consumer membership limit exceeded"
        );
    }
    transaction.commit()?;
    Ok(())
}

pub(super) struct Journal {
    pub source: ComponentId,
    pub epoch: Uuid,
    pub head: u64,
    pub retired: u64,
    pub bytes: u64,
    pub sent: u64,
    pub schema: String,
    generations: Generations,
    positions: BTreeMap<ComponentId, u64>,
    limits: SqliteReplayConfig,
}

impl Journal {
    pub fn open(
        connection: &Connection,
        config: &SqliteConfig,
        source: ComponentId,
        stream: &StreamId,
        progress: (&Reader, &Observed),
        schema: String,
        bootstrap: Option<Uuid>,
    ) -> Result<Self> {
        Self::open_mode(
            connection, config, source, stream, progress, schema, bootstrap, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_mode(
        connection: &Connection,
        config: &SqliteConfig,
        source: ComponentId,
        stream: &StreamId,
        progress: (&Reader, &Observed),
        schema: String,
        bootstrap: Option<Uuid>,
        snapshot_only: bool,
    ) -> Result<Self> {
        let (owner, snapshot) = progress;
        let limits = config
            .replay
            .clone()
            .context("SQLite journal settings missing")?;
        let binding = match owner {
            Reader::Single(owner) => binding(config, &source, stream, owner, bootstrap.is_some())?,
            Reader::Group(owners) => {
                group_binding(config, &source, stream, owners, bootstrap.is_some())?
            }
        };
        let existing: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='__drasi_native_state_v1')",
            [],
            |row| row.get(0),
        )?;
        let transaction = connection.unchecked_transaction()?;
        if !existing {
            anyhow::ensure!(
                !snapshot.has_checkpoint(&source),
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
            if matches!(owner, Reader::Group(_)) {
                transaction.execute_batch(
                    "CREATE TABLE __drasi_native_consumers_v1(
                     consumer TEXT PRIMARY KEY, boundary INTEGER NOT NULL,
                     position INTEGER NOT NULL, confirmed INTEGER NOT NULL,
                     active INTEGER NOT NULL);",
                )?;
            }
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
            generations: snapshot.generations(),
            positions: BTreeMap::new(),
            limits,
            schema,
        };
        if !snapshot_only {
            let resume = journal.position(snapshot)?;
            anyhow::ensure!(
                resume >= retired && resume <= head,
                "native SQLite consumer cursor is outside retained history"
            );
            journal.sent = resume;
        }
        transaction.commit()?;
        if let Observed::Group(snapshot) = snapshot {
            anyhow::ensure!(
                matches!(owner, Reader::Group(_)),
                "SQLite progress mode mismatch"
            );
            let has_members: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='__drasi_native_consumers_v1')",
                [], |row| row.get(0),
            )?;
            anyhow::ensure!(
                has_members,
                "native SQLite shared membership metadata is missing"
            );
            let count: u64 = connection.query_row(
                "SELECT count(*) FROM __drasi_native_consumers_v1",
                [],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                count <= MAX_CONSUMER_PROGRESS_MEMBERS as u64,
                "native SQLite recorded consumer membership limit exceeded"
            );
            if !snapshot_only {
                journal.bind_members(connection, snapshot, bootstrap.is_some())?;
            }
        }
        Ok(journal)
    }

    fn member_position(&self, snapshot: &SourceProgressSnapshot, generation: u64) -> Result<u64> {
        anyhow::ensure!(
            snapshot.failure.is_none()
                && snapshot.persistent
                && snapshot.reset_generation == generation,
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

    pub fn position(&self, snapshot: &Observed) -> Result<u64> {
        match (&self.generations, snapshot) {
            (Generations::Single(generation), Observed::Single(snapshot)) => {
                self.member_position(snapshot, *generation)
            }
            (Generations::Group(generations), Observed::Group(snapshot)) => {
                anyhow::ensure!(
                    generations.len() == snapshot.consumers.len()
                        && generations.keys().eq(snapshot.consumers.keys()),
                    "native SQLite consumer membership changed during replay"
                );
                let mut oldest = self.head;
                for (consumer, state) in &snapshot.consumers {
                    anyhow::ensure!(
                        state.ready && state.admitting && state.recovered,
                        "native SQLite consumer {consumer} is not ready for live input"
                    );
                    let position = self
                        .member_position(state, generations[consumer])
                        .with_context(|| format!("native SQLite consumer {consumer}"))?;
                    anyhow::ensure!(
                        position <= self.head,
                        "native SQLite consumer {consumer} is ahead of the journal"
                    );
                    anyhow::ensure!(
                        self.positions
                            .get(consumer)
                            .is_none_or(|previous| position >= *previous),
                        "native SQLite consumer {consumer} progress moved backwards"
                    );
                    oldest = oldest.min(position);
                }
                Ok(oldest)
            }
            _ => anyhow::bail!("native SQLite progress binding mode changed"),
        }
    }

    fn member_positions(
        &self,
        snapshot: &drasi_computation_contracts::ConsumerProgressSnapshot,
    ) -> Result<BTreeMap<ComponentId, u64>> {
        let Generations::Group(generations) = &self.generations else {
            anyhow::bail!("native SQLite journal has no consumer group");
        };
        snapshot
            .consumers
            .iter()
            .map(|(consumer, state)| {
                let generation = generations
                    .get(consumer)
                    .context("SQLite consumer membership changed")?;
                Ok((consumer.clone(), self.member_position(state, *generation)?))
            })
            .collect()
    }

    fn bind_members(
        &mut self,
        connection: &Connection,
        snapshot: &drasi_computation_contracts::ConsumerProgressSnapshot,
        coordinated: bool,
    ) -> Result<()> {
        let positions = self.member_positions(snapshot)?;
        let transaction = connection.unchecked_transaction()?;
        let stored = {
            let count: u64 = transaction.query_row(
                "SELECT count(*) FROM __drasi_native_consumers_v1",
                [],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                count <= MAX_CONSUMER_PROGRESS_MEMBERS as u64,
                "native SQLite recorded consumer membership limit exceeded"
            );
            let mut statement = transaction.prepare(
                "SELECT consumer,boundary,position,confirmed,active FROM __drasi_native_consumers_v1"
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, bool>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (consumer, _, position, confirmed, active) in &stored {
            let id = ComponentId::try_new(consumer.as_str())?;
            if *active && !positions.contains_key(&id) {
                anyhow::ensure!(*confirmed && *position == self.head,
                    "native SQLite cannot remove consumer {consumer} before its retained input is fully handled");
            }
        }
        for (consumer, position) in &positions {
            if let Some((_, boundary, _, _, _)) =
                stored.iter().find(|(name, ..)| name == consumer.as_str())
            {
                anyhow::ensure!(
                    *position >= *boundary,
                    "native SQLite consumer {consumer} lost its initialization boundary"
                );
            } else {
                anyhow::ensure!(!coordinated && !snapshot.consumers[consumer].checkpoints
                    .contains_key(&SourceProgressKey::Source(self.source.to_string())),
                    "native SQLite consumer {consumer} requires its coordinated snapshot enrollment");
                anyhow::ensure!(
                    transaction.execute(
                        "INSERT INTO __drasi_native_consumers_v1 SELECT ?1,0,?2,1,1 WHERE
                     (SELECT count(*) FROM __drasi_native_consumers_v1) < ?3",
                        params![
                            consumer.as_str(),
                            position,
                            MAX_CONSUMER_PROGRESS_MEMBERS as u64
                        ],
                    )? == 1,
                    "native SQLite recorded consumer membership limit exceeded"
                );
            }
        }
        transaction.execute("UPDATE __drasi_native_consumers_v1 SET active=0", [])?;
        for (consumer, position) in &positions {
            transaction.execute(
                "UPDATE __drasi_native_consumers_v1 SET position=?1,confirmed=1,active=1 WHERE consumer=?2",
                params![position, consumer.as_str()],
            )?;
        }
        transaction.commit()?;
        self.positions = positions;
        Ok(())
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

    pub fn feedback(&mut self, connection: &Connection, snapshot: &Observed) -> Result<()> {
        let position = self.position(snapshot)?;
        anyhow::ensure!(
            position >= self.retired && position <= self.sent,
            "native SQLite consumer progress moved outside emitted history"
        );
        let positions = match snapshot {
            Observed::Group(snapshot) => Some(self.member_positions(snapshot)?),
            Observed::Single(_) => None,
        };
        if !snapshot.ready()
            || (position == self.retired
                && positions
                    .as_ref()
                    .is_none_or(|positions| positions == &self.positions))
        {
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
        if let Some(positions) = &positions {
            for (consumer, position) in positions {
                anyhow::ensure!(transaction.execute(
                    "UPDATE __drasi_native_consumers_v1 SET position=?1 WHERE consumer=?2 AND active=1 AND confirmed=1",
                    params![position, consumer.as_str()],
                )? == 1, "native SQLite consumer membership changed outside its owner");
            }
        }
        transaction.commit()?;
        if let Some(positions) = positions {
            self.positions = positions;
        }
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
