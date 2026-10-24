// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    catalog::{self, Tables},
    file_number, MySqlConfig, MySqlOutput,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_computation_contracts::{
    ChangeEnvelope, ChangeOperation, ComponentId, GraphChangeCodec, OutputEnvelope, PortId,
    SourceTransactionBuilder, SourceTransactionError, StreamId,
};
use drasi_core_types::models::SourceChange;
use mysql_common::binlog::{
    consts::{BinlogChecksumAlg, BinlogVersion, EventType, RowsEventFlags},
    events::{BinlogEventFooter, Event, EventData, FormatDescriptionEvent, TableMapEvent},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::time::Instant;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub version: u32,
    pub binding: [u8; 32],
    pub file: String,
    pub start: u32,
    pub end: u32,
    pub digest: [u8; 32],
}

impl Position {
    pub fn sequence(&self) -> Result<u64> {
        anyhow::ensure!(
            self.start >= 4
                && self.end > self.start
                && matches!(self.version, 1 | 2)
                && (self.version != 2 || self.start == 4),
            "invalid MySQL transaction cursor"
        );
        Ok((u64::from(file_number(&self.file)?) << 32) | u64::from(self.end))
    }
}

struct Pending {
    start: u32,
    began: bool,
    builder: Option<SourceTransactionBuilder>,
    digest: Option<Sha256>,
    commit_time: Option<DateTime<Utc>>,
}

struct Table {
    selected: Option<TableMapEvent<'static>>,
    bytes: usize,
}

pub struct Binlog {
    source: ComponentId,
    stream: StreamId,
    config: MySqlConfig,
    tables: Tables,
    binding: [u8; 32],
    file: String,
    offset: u32,
    format: FormatDescriptionEvent<'static>,
    described: bool,
    maps: BTreeMap<u64, Table>,
    map_bytes: usize,
    pending: Option<Pending>,
    anchor: Option<Position>,
    sequence: Arc<AtomicU64>,
    failed: bool,
    pub admitted: u64,
    row_semantics: bool,
    gtid_start: bool,
    positional: bool,
}

pub struct Start {
    pub source: ComponentId,
    pub stream: StreamId,
    pub config: MySqlConfig,
    pub tables: Tables,
    pub binding: [u8; 32],
    pub file: String,
    pub offset: u32,
    pub anchor: Option<Position>,
    pub sequence: Arc<AtomicU64>,
}

pub enum HistoryMode {
    Live,
    Verify,
}

impl Binlog {
    pub fn after_snapshot(&mut self, format: FormatDescriptionEvent<'static>, sequence: u64) {
        self.format = format;
        self.described = true;
        self.admitted = sequence;
    }
    pub fn new(start: Start) -> Result<Self> {
        let admitted = start
            .anchor
            .as_ref()
            .map(Position::sequence)
            .transpose()?
            .unwrap_or(0);
        Ok(Self {
            source: start.source,
            stream: start.stream,
            config: start.config,
            tables: start.tables,
            binding: start.binding,
            file: start.file,
            offset: start.offset,
            anchor: start.anchor,
            sequence: start.sequence,
            format: FormatDescriptionEvent::new(BinlogVersion::Version4).with_footer(
                BinlogEventFooter::new(BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_CRC32),
            ),
            described: false,
            maps: BTreeMap::new(),
            map_bytes: 0,
            pending: None,
            failed: false,
            admitted,
            row_semantics: false,
            gtid_start: false,
            positional: false,
        })
    }

    pub fn with_row_semantics(mut self) -> Self {
        self.row_semantics = true;
        self
    }

    pub fn with_positional_metadata(mut self) -> Self {
        self.positional = true;
        self
    }

    pub fn with_gtid_start(mut self) -> Self {
        self.gtid_start = true;
        self
    }

    pub fn new_table(&self, raw: &[u8]) -> Result<Option<String>> {
        if !self.config.tables.is_empty()
            || self.config.replay.is_some()
            || self.config.output != MySqlOutput::Changes
            || raw.get(4) != Some(&(EventType::TABLE_MAP_EVENT as u8))
        {
            return Ok(None);
        }
        let event = frame(&self.format, raw, self.config.max_protocol_bytes.get())?;
        validate_table_bytes(event.data(), !self.positional)?;
        let table: TableMapEvent<'_> = event.read_event()?;
        let name = std::str::from_utf8(table.table_name_raw())?;
        if std::str::from_utf8(table.database_name_raw())? == self.config.connection.database
            && !self.tables.contains_key(name)
        {
            return Ok(Some(name.to_owned()));
        }
        Ok(None)
    }

    pub fn include_table(&mut self, table: String, columns: Vec<catalog::Column>) -> Result<()> {
        anyhow::ensure!(
            self.config.tables.is_empty()
                && self.config.replay.is_none()
                && self.config.output == MySqlOutput::Changes,
            "live table discovery requires untracked database-wide row mode"
        );
        self.tables.insert(table, columns);
        Ok(())
    }

    pub fn catalog(&self) -> &catalog::Tables {
        &self.tables
    }

    /// Reads exactly the captured file prefix, leaving replication positioned at its
    /// end. Hashing is bounded-memory and happens after releasing the global lock.
    pub async fn snapshot_history(
        connection: &mut super::connection::Connection,
        config: &MySqlConfig,
        file: &str,
        boundary: u32,
        mode: HistoryMode,
    ) -> Result<(FormatDescriptionEvent<'static>, [u8; 32])> {
        connection
            .execute("SET @master_binlog_checksum='CRC32'")
            .await?;
        match mode {
            HistoryMode::Live => {
                connection
                    .start_binlog(config.connection.server_id.get(), file, 4)
                    .await?;
            }
            HistoryMode::Verify => connection.start_binlog_history(file, 4).await?,
        }
        let mut format = FormatDescriptionEvent::new(BinlogVersion::Version4).with_footer(
            BinlogEventFooter::new(BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_CRC32),
        );
        let mut offset = 4;
        let mut described = false;
        let mut rotated = false;
        let mut hash = Sha256::new();
        while offset < boundary {
            let raw = connection.event().await?;
            let event = frame(&format, &raw, config.max_protocol_bytes.get())?;
            match event.header().event_type()? {
                EventType::ROTATE_EVENT if !described && !rotated => {
                    let rotate: mysql_common::binlog::events::RotateEvent<'_> =
                        event.read_event()?;
                    anyhow::ensure!(
                        rotate.name_raw() == file.as_bytes() && rotate.position() == 4,
                        "MySQL snapshot history file changed"
                    );
                    rotated = true;
                    continue;
                }
                EventType::FORMAT_DESCRIPTION_EVENT if !described => {
                    let found: FormatDescriptionEvent<'_> = event.read_event()?;
                    anyhow::ensure!(
                        found.binlog_version() == BinlogVersion::Version4
                            && found.event_header_length() == 19,
                        "unsupported snapshot binlog format"
                    );
                    format = found.into_owned().with_footer(event.footer());
                    described = true;
                    // The in-use flag/checksum change when MySQL closes a binlog file.
                    hash.update(&raw[19..raw.len() - 4]);
                }
                _ => {
                    anyhow::ensure!(described, "snapshot history has no format");
                    hash.update(&raw);
                }
            }
            anyhow::ensure!(
                event.header().log_pos()
                    == offset
                        .checked_add(event.header().event_size())
                        .context("binlog position overflow")?
                    && event.header().log_pos() <= boundary,
                "MySQL snapshot boundary is unavailable or not an event boundary"
            );
            offset = event.header().log_pos();
        }
        anyhow::ensure!(
            described && offset == boundary,
            "invalid snapshot history boundary"
        );
        Ok((format, hash.finalize().into()))
    }

    pub async fn verify_positional_catalog_history(
        connection: &mut super::connection::Connection,
        config: &MySqlConfig,
        file: &str,
        start: u32,
        boundary: u32,
    ) -> Result<()> {
        anyhow::ensure!(
            start >= 4 && boundary >= start,
            "invalid positional history range"
        );
        if start == boundary {
            return Ok(());
        }
        connection
            .execute("SET @master_binlog_checksum='CRC32'")
            .await?;
        connection.start_binlog_history(file, start).await?;
        let mut format = FormatDescriptionEvent::new(BinlogVersion::Version4).with_footer(
            BinlogEventFooter::new(BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_CRC32),
        );
        let mut described = false;
        let mut rotated = false;
        let mut offset = start;
        while offset < boundary {
            let raw = connection.event().await?;
            let event = frame(&format, &raw, config.max_protocol_bytes.get())?;
            let kind = event.header().event_type()?;
            if kind == EventType::ROTATE_EVENT && !described && !rotated {
                let rotate: mysql_common::binlog::events::RotateEvent<'_> = event.read_event()?;
                anyhow::ensure!(
                    rotate.name_raw() == file.as_bytes() && rotate.position() == u64::from(start),
                    "positional history file changed"
                );
                rotated = true;
                continue;
            }
            if kind == EventType::FORMAT_DESCRIPTION_EVENT && !described {
                let found: FormatDescriptionEvent<'_> = event.read_event()?;
                anyhow::ensure!(
                    found.binlog_version() == BinlogVersion::Version4
                        && found.event_header_length() == 19,
                    "unsupported positional history format"
                );
                format = found.into_owned().with_footer(event.footer());
                described = true;
                if start > 4 {
                    continue;
                }
            } else {
                anyhow::ensure!(described, "positional history has no format");
            }
            if kind == EventType::QUERY_EVENT {
                let query: mysql_common::binlog::events::QueryEvent<'_> = event.read_event()?;
                let sql = std::str::from_utf8(query.query_raw())?;
                anyhow::ensure!(
                    positional_statement_preserves_catalog(
                        sql, std::str::from_utf8(query.schema_raw())?, &config.connection.database,
                    ),
                    "positional MySQL history contains a catalog-changing or unrecognized statement (kind {}, schema {}); an explicit matching-schema start is required",
                    sql.trim_start().chars().take_while(char::is_ascii_alphabetic).take(16).collect::<String>(),
                    query.schema(),
                );
            }
            let end = offset
                .checked_add(event.header().event_size())
                .context("positional history position overflow")?;
            anyhow::ensure!(
                (event.header().log_pos() == end
                    || (event.header().log_pos() == 0
                        && matches!(kind, EventType::STOP_EVENT | EventType::ROTATE_EVENT)))
                    && end <= boundary,
                "positional history is truncated or its boundary changed"
            );
            offset = end;
        }
        anyhow::ensure!(
            described && offset == boundary,
            "incomplete positional catalog history"
        );
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.described
    }
    pub fn has_replay_anchor(&self) -> bool {
        self.anchor.is_some()
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().and_then(|pending| {
            pending
                .builder
                .as_ref()
                .map(SourceTransactionBuilder::deadline)
        })
    }

    pub fn decode(&mut self, raw: &[u8]) -> Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            !self.failed,
            "MySQL decoder is fenced after a failed transaction"
        );
        let result = self.decode_inner(raw);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn decode_inner(&mut self, raw: &[u8]) -> Result<Vec<OutputEnvelope>> {
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(SourceTransactionError::Deadline.into());
        }
        let event = frame(&self.format, raw, self.config.max_protocol_bytes.get())?;
        let kind = event.header().event_type()?;
        if kind == EventType::FORMAT_DESCRIPTION_EVENT {
            anyhow::ensure!(
                self.pending.is_none(),
                "MySQL format changed inside a transaction"
            );
            let format: FormatDescriptionEvent<'_> = event.read_event()?;
            anyhow::ensure!(
                format.binlog_version() == BinlogVersion::Version4
                    && format.event_header_length() == 19,
                "unsupported MySQL binlog format"
            );
            for (kind, length) in [
                (EventType::QUERY_EVENT, 13),
                (EventType::ROTATE_EVENT, 8),
                (EventType::TABLE_MAP_EVENT, 8),
                (EventType::WRITE_ROWS_EVENT, 10),
                (EventType::UPDATE_ROWS_EVENT, 10),
                (EventType::DELETE_ROWS_EVENT, 10),
            ] {
                anyhow::ensure!(
                    format.get_event_type_header_length(kind) == length,
                    "unsupported MySQL event header"
                );
            }
            self.format = format.into_owned().with_footer(event.footer());
            self.described = true;
            return Ok(Vec::new());
        }
        if kind == EventType::ROTATE_EVENT {
            anyhow::ensure!(
                self.pending.is_none(),
                "MySQL rotated inside an incomplete transaction"
            );
            let rotate: mysql_common::binlog::events::RotateEvent<'_> = event.read_event()?;
            let next = std::str::from_utf8(rotate.name_raw())?.to_string();
            let offset: u32 = rotate.position().try_into()?;
            let same =
                next == self.file && offset == self.offset && event.header().timestamp() == 0;
            let first_gtid_file = self.gtid_start && self.file.is_empty() && !self.described;
            anyhow::ensure!(
                offset >= 4
                    && (same
                        || (first_gtid_file && file_number(&next).is_ok())
                        || (file_number(&next)? > file_number(&self.file)?
                            && next.rsplit_once('.').map(|part| part.0)
                                == self.file.rsplit_once('.').map(|part| part.0))),
                "MySQL binlog history moved backwards or changed identity"
            );
            self.file = next;
            self.offset = offset;
            self.clear_maps();
            return Ok(Vec::new());
        }
        anyhow::ensure!(
            self.described,
            "MySQL event arrived before its format description"
        );
        if kind == EventType::HEARTBEAT_EVENT {
            return Ok(Vec::new());
        }
        if kind == EventType::PREVIOUS_GTIDS_EVENT {
            anyhow::ensure!(
                self.pending.is_none(),
                "MySQL history metadata inside transaction"
            );
            self.offset = self.offset.max(event.header().log_pos());
            return Ok(Vec::new());
        }
        if kind == EventType::STOP_EVENT {
            anyhow::ensure!(
                self.pending.is_none(),
                "MySQL stopped inside an incomplete transaction"
            );
            let end = self
                .offset
                .checked_add(event.header().event_size())
                .context("MySQL stopped-file position overflow")?;
            anyhow::ensure!(
                event.header().log_pos() == 0 || event.header().log_pos() == end,
                "noncontiguous MySQL stopped-file metadata"
            );
            self.offset = end;
            return Ok(Vec::new());
        }
        anyhow::ensure!(
            matches!(
                kind,
                EventType::GTID_EVENT
                    | EventType::ANONYMOUS_GTID_EVENT
                    | EventType::QUERY_EVENT
                    | EventType::XID_EVENT
                    | EventType::TABLE_MAP_EVENT
                    | EventType::WRITE_ROWS_EVENT
                    | EventType::UPDATE_ROWS_EVENT
                    | EventType::DELETE_ROWS_EVENT
                    | EventType::ROWS_QUERY_EVENT
            ),
            "unsupported MySQL binlog event {kind:?}; no checkpoint was advanced"
        );
        let end = event.header().log_pos();
        let start = end
            .checked_sub(event.header().event_size())
            .context("invalid MySQL event position")?;
        let skipped_gtids = self.gtid_start
            && self.pending.is_none()
            && matches!(
                kind,
                EventType::GTID_EVENT | EventType::ANONYMOUS_GTID_EVENT
            );
        anyhow::ensure!(
            (start == self.offset || (skipped_gtids && start > self.offset)) && end > start,
            "noncontiguous MySQL binlog history"
        );
        if matches!(
            kind,
            EventType::GTID_EVENT | EventType::ANONYMOUS_GTID_EVENT
        ) {
            anyhow::ensure!(
                self.pending.is_none(),
                "new MySQL transaction before prior commit"
            );
            if let Some(anchor) = &self.anchor {
                anyhow::ensure!(
                    anchor.file == self.file && anchor.start == start,
                    "MySQL replay anchor was replaced"
                );
            }
            let builder = (self.config.output == MySqlOutput::Transactions)
                .then(|| {
                    SourceTransactionBuilder::new(self.source.as_str(), self.config.transactions)
                })
                .transpose()?;
            self.pending = Some(Pending {
                start,
                began: false,
                builder,
                digest: self.config.replay.map(|_| Sha256::new()),
                commit_time: None,
            });
        }
        let pending = self
            .pending
            .as_mut()
            .context("MySQL event outside a transaction")?;
        if let Some(digest) = &mut pending.digest {
            digest.update(raw);
        }
        self.offset = end;
        if kind == EventType::TABLE_MAP_EVENT {
            validate_table_bytes(event.data(), !self.positional)?;
        }
        let mut output = Vec::new();
        let mut output_bytes = 0usize;
        let data = event.read_data()?.context("unknown MySQL event")?;
        let commit_timestamp = match &data {
            EventData::GtidEvent(gtid) => Some(gtid.immediate_commit_timestamp()),
            EventData::AnonymousGtidEvent(gtid) => Some(gtid.0.immediate_commit_timestamp()),
            _ => None,
        };
        if let Some(timestamp) = commit_timestamp {
            if timestamp != 0 {
                pending.commit_time = Some(
                    DateTime::from_timestamp_micros(
                        timestamp
                            .try_into()
                            .context("MySQL commit timestamp exceeds signed time")?,
                    )
                    .context("invalid MySQL commit timestamp")?,
                );
            } else {
                anyhow::ensure!(
                    !matches!(
                        self.config.start,
                        super::MySqlStartPosition::Snapshot { .. }
                    ),
                    "MySQL snapshot handover requires transaction commit timestamps"
                );
            }
        }
        match data {
            EventData::GtidEvent(_)
            | EventData::AnonymousGtidEvent(_)
            | EventData::RowsQueryEvent(_) => {}
            EventData::QueryEvent(query) => {
                let sql = std::str::from_utf8(query.query_raw())?;
                match sql {
                    "BEGIN" => {
                        anyhow::ensure!(!pending.began, "nested MySQL BEGIN");
                        pending.began = true;
                    }
                    "COMMIT" => output.extend(self.commit(end, event.header().timestamp())?),
                    "ROLLBACK" if self.row_semantics => {
                        // MySQL omits rolled-back InnoDB rows but still logs
                        // nontransactional writes from the same transaction.
                        output.extend(self.commit(end, event.header().timestamp())?)
                    }
                    "ROLLBACK" => anyhow::bail!("unexpected MySQL rollback in committed binlog"),
                    _ => {
                        let database = std::str::from_utf8(query.schema_raw())?;
                        anyhow::ensure!(
                            !self.positional || positional_statement_preserves_catalog(
                                sql, database, &self.config.connection.database,
                            ),
                            "positional MySQL catalog changed; explicit matching-schema recovery is required"
                        );
                        let creation = sql
                            .split_ascii_whitespace()
                            .next()
                            .is_some_and(|word| word.eq_ignore_ascii_case("CREATE"));
                        anyhow::ensure!(
                            !pending.began
                                && (creation
                                    || (!database.is_empty()
                                        && database != self.config.connection.database)),
                            "MySQL DDL or statement-based change requires explicit schema recovery"
                        );
                        output.extend(self.commit(end, event.header().timestamp())?);
                    }
                }
            }
            EventData::XidEvent(_) => output.extend(self.commit(end, event.header().timestamp())?),
            EventData::TableMapEvent(table) => {
                anyhow::ensure!(pending.began, "MySQL table map before BEGIN");
                let name = std::str::from_utf8(table.table_name_raw())?;
                anyhow::ensure!(
                    !self.config.tables.is_empty()
                        || std::str::from_utf8(table.database_name_raw())? != self.config.connection.database
                        || self.tables.contains_key(name),
                    "MySQL database-wide table selection changed; explicit schema recovery is required"
                );
                let selected = std::str::from_utf8(table.database_name_raw())?
                    == self.config.connection.database
                    && self.tables.contains_key(name);
                if selected {
                    validate_table(&table)?;
                }
                if let Some(previous) = self.maps.remove(&table.table_id()) {
                    self.map_bytes -= previous.bytes;
                }
                let bytes = if selected { raw.len() } else { 64 };
                self.map_bytes = self
                    .map_bytes
                    .checked_add(bytes)
                    .context("MySQL table cache overflow")?;
                anyhow::ensure!(
                    self.map_bytes <= self.config.max_protocol_bytes.get(),
                    "MySQL table metadata cache exceeds byte limit"
                );
                self.maps.insert(
                    table.table_id(),
                    Table {
                        selected: selected.then(|| table.into_owned()),
                        bytes,
                    },
                );
            }
            EventData::RowsEvent(rows) => {
                anyhow::ensure!(
                    pending.began && (1..=4096).contains(&rows.num_columns()),
                    "invalid MySQL row column count or transaction"
                );
                let table = self
                    .maps
                    .get(&rows.table_id())
                    .context("MySQL row references an unknown table")?;
                if let Some(table) = &table.selected {
                    anyhow::ensure!(
                        rows.num_columns() == table.columns_count()
                            && rows.columns_before_image().is_none_or(|mask| mask.all())
                            && rows.columns_after_image().is_none_or(|mask| mask.all()),
                        "native MySQL requires full before/after row images"
                    );
                    let columns = self
                        .tables
                        .get(std::str::from_utf8(table.table_name_raw())?)
                        .context("missing selected table")?;
                    let keys = if self.positional {
                        catalog::positional_keys(
                            &self.config,
                            std::str::from_utf8(table.table_name_raw())?,
                            columns.len(),
                        )?
                    } else {
                        catalog::key_columns(
                            &self.config,
                            std::str::from_utf8(table.table_name_raw())?,
                            columns,
                        )?
                    };
                    let decoder = crate::decoder::MySqlDecoder::new(self.source.as_str(), &[]);
                    let event_time = if self.positional {
                        Utc::now()
                    } else {
                        pending
                            .commit_time
                            .unwrap_or(time(event.header().timestamp())?)
                    };
                    let timestamp: u64 = event_time.timestamp_millis().try_into()?;
                    let map = |row: &mysql_common::binlog::row::BinlogRow| {
                        if self.positional {
                            catalog::validate_positional_row(columns, row)?;
                            return decoder.native_positional_element(
                                table,
                                row,
                                &keys,
                                columns,
                                timestamp,
                                self.config.max_protocol_bytes.get(),
                            );
                        }
                        catalog::validate_row(columns, row)?;
                        decoder.native_element(
                            table,
                            row,
                            &keys,
                            timestamp,
                            self.config.max_protocol_bytes.get(),
                        )
                    };
                    for row in rows.rows(table) {
                        let (before, after) = row?;
                        let changes = match (before.as_ref(), after.as_ref()) {
                            (None, Some(after)) => vec![SourceChange::Insert {
                                element: map(after)?,
                            }],
                            (Some(before), None) => vec![SourceChange::Delete {
                                metadata: map(before)?.get_metadata().clone(),
                            }],
                            (Some(before), Some(after)) => {
                                let before = map(before)?;
                                let after = map(after)?;
                                if before.get_reference() != after.get_reference() {
                                    vec![
                                        SourceChange::Delete {
                                            metadata: before.get_metadata().clone(),
                                        },
                                        SourceChange::Insert { element: after },
                                    ]
                                } else {
                                    vec![SourceChange::Update { element: after }]
                                }
                            }
                            _ => anyhow::bail!("empty MySQL row change"),
                        };
                        for change in changes {
                            if let Some(builder) = &mut pending.builder {
                                builder.push(change)?;
                            } else {
                                let envelope = GraphChangeCodec::encode_change(
                                    change,
                                    self.stream.clone(),
                                    next(&self.sequence)?,
                                    Some(event_time),
                                )?;
                                output_bytes = output_bytes
                                    .checked_add(envelope_bytes(&envelope)?)
                                    .context("MySQL decoded batch size overflow")?;
                                anyhow::ensure!(
                                    output_bytes <= self.config.max_protocol_bytes.get(),
                                    "MySQL decoded batch exceeds protocol byte limit"
                                );
                                output.push(OutputEnvelope {
                                    port: PortId::try_new("out")?,
                                    envelope,
                                });
                            }
                        }
                    }
                }
                if rows.flags().contains(RowsEventFlags::STMT_END) {
                    self.clear_maps();
                }
            }
            _ => anyhow::bail!("unsupported MySQL transaction event"),
        }
        Ok(output)
    }

    fn commit(&mut self, end: u32, timestamp: u32) -> Result<Option<OutputEnvelope>> {
        let pending = self
            .pending
            .take()
            .context("MySQL commit without transaction")?;
        self.clear_maps();
        let Some(builder) = pending.builder else {
            return Ok(None);
        };
        let timestamp = if self.positional {
            Utc::now()
        } else {
            pending.commit_time.unwrap_or(time(timestamp)?)
        };
        let position = Position {
            version: 1,
            binding: self.binding,
            file: self.file.clone(),
            start: pending.start,
            end,
            digest: pending
                .digest
                .map(|hash| hash.finalize().into())
                .unwrap_or([0; 32]),
        };
        let logical = position.sequence()?;
        let transaction = builder.commit(
            Bytes::copy_from_slice(&logical.to_be_bytes()),
            Bytes::from(serde_json::to_vec(&position)?),
        )?;
        if let Some(anchor) = self.anchor.take() {
            anyhow::ensure!(
                position == anchor,
                "MySQL committed replay anchor is missing or has different contents"
            );
            self.admitted = logical;
            return Ok(None);
        }
        anyhow::ensure!(
            logical > self.admitted,
            "MySQL transaction sequence moved backwards"
        );
        let transport = next(&self.sequence)?;
        let envelope = if self.config.replay.is_some() {
            transaction.into_replay_envelope(self.stream.clone(), logical, transport, timestamp)?
        } else {
            transaction.into_envelope(self.stream.clone(), transport, timestamp)?
        };
        self.admitted = logical;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }))
    }

    fn clear_maps(&mut self) {
        self.maps.clear();
        self.map_bytes = 0;
    }
}

fn time(timestamp: u32) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp(timestamp.into(), 0).context("invalid MySQL event timestamp")
}

fn next(sequence: &AtomicU64) -> Result<u64> {
    sequence
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map(|value| value + 1)
        .map_err(|_| anyhow::anyhow!("MySQL transport sequence exhausted"))
}

pub fn envelope_bytes(envelope: &ChangeEnvelope) -> Result<usize> {
    let mut total = 0usize;
    for operation in envelope.changes().operations() {
        let payload = match operation {
            ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                after.payload().len()
            }
            ChangeOperation::Deleted { before, .. } => {
                before.as_ref().map_or(0, |record| record.payload().len())
            }
        };
        total = total
            .checked_add(payload)
            .and_then(|total| total.checked_add(operation.identity().value().len()))
            .and_then(|total| total.checked_add(512))
            .context("MySQL decoded batch size overflow")?;
    }
    Ok(total)
}

fn frame(format: &FormatDescriptionEvent<'_>, raw: &[u8], limit: usize) -> Result<Event> {
    anyhow::ensure!(
        raw.len() >= 23 && raw.len() <= limit,
        "invalid or oversized MySQL binlog frame"
    );
    let size = u32::from_le_bytes(raw[9..13].try_into()?);
    anyhow::ensure!(
        size as usize == raw.len(),
        "MySQL binlog frame size mismatch"
    );
    if raw[4] == EventType::FORMAT_DESCRIPTION_EVENT as u8 {
        anyhow::ensure!(
            raw.len() >= 19 + 57 + 5,
            "truncated MySQL format description"
        );
    }
    let event = Event::read(format, raw)?;
    let algorithm = event
        .footer()
        .get_checksum_alg()?
        .context("missing MySQL checksum algorithm")?;
    anyhow::ensure!(
        algorithm == BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_CRC32,
        "native MySQL requires CRC32 binlog checksums"
    );
    anyhow::ensure!(
        event.checksum().map(u32::from_le_bytes) == Some(event.calc_checksum(algorithm)),
        "MySQL binlog checksum mismatch"
    );
    Ok(event)
}

fn take<'a>(input: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    anyhow::ensure!(count <= input.len(), "truncated MySQL table metadata");
    let (value, rest) = input.split_at(count);
    *input = rest;
    Ok(value)
}

fn length(input: &mut &[u8]) -> Result<usize> {
    super::connection::length(input)?.context("NULL MySQL metadata length")
}

fn positional_statement_preserves_catalog(sql: &str, schema: &str, database: &str) -> bool {
    if !schema.eq_ignore_ascii_case(database)
        && !sql
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|word| word.eq_ignore_ascii_case(database))
    {
        return true;
    }
    let mut words = sql.split_ascii_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if [
        "BEGIN", "COMMIT", "ROLLBACK", "CREATE", "SET", "GRANT", "REVOKE",
    ]
    .iter()
    .any(|verb| first.eq_ignore_ascii_case(verb))
    {
        return true;
    }
    matches!(words.next(), Some(object)
        if ["USER", "ROLE", "PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"]
            .iter().any(|kind| object.eq_ignore_ascii_case(kind)))
        && ["ALTER", "DROP", "RENAME"]
            .iter()
            .any(|verb| first.eq_ignore_ascii_case(verb))
}

fn validate_table_bytes(mut input: &[u8], require_names: bool) -> Result<()> {
    take(&mut input, 8)?;
    for _ in 0..2 {
        let count = usize::from(take(&mut input, 1)?[0]);
        std::str::from_utf8(take(&mut input, count)?)?;
        anyhow::ensure!(
            take(&mut input, 1)? == [0],
            "invalid MySQL table name terminator"
        );
    }
    let columns = length(&mut input)?;
    anyhow::ensure!(
        (1..=4096).contains(&columns),
        "MySQL column count exceeds supported limit"
    );
    take(&mut input, columns)?;
    let count = length(&mut input)?;
    take(&mut input, count)?;
    take(&mut input, columns.div_ceil(8))?;
    let mut seen = [false; 13];
    while !input.is_empty() {
        let tag = usize::from(take(&mut input, 1)?[0]);
        anyhow::ensure!(
            (1..=12).contains(&tag) && !seen[tag],
            "unknown or duplicate MySQL optional metadata"
        );
        seen[tag] = true;
        let count = length(&mut input)?;
        let mut field = take(&mut input, count)?;
        if matches!(tag, 5 | 6) {
            while !field.is_empty() {
                let values = length(&mut field)?;
                anyhow::ensure!(values <= field.len(), "invalid MySQL ENUM/SET value count");
                for _ in 0..values {
                    let count = length(&mut field)?;
                    std::str::from_utf8(take(&mut field, count)?)?;
                }
            }
        }
    }
    anyhow::ensure!(
        !require_names || seen[4],
        "native MySQL requires full column names"
    );
    Ok(())
}

fn validate_table(table: &TableMapEvent<'_>) -> Result<()> {
    use mysql_common::constants::ColumnType::*;
    for index in 0..table.columns_count() as usize {
        let kind = table
            .get_column_type(index)?
            .context("missing MySQL column type")?;
        let metadata = table
            .get_column_metadata(index)
            .context("missing MySQL column metadata")?;
        let valid = match kind {
            MYSQL_TYPE_NEWDECIMAL => {
                metadata.len() == 2
                    && (1..=65).contains(&metadata[0])
                    && metadata[1] <= metadata[0]
                    && metadata[1] <= 30
            }
            MYSQL_TYPE_TIMESTAMP2 | MYSQL_TYPE_DATETIME2 | MYSQL_TYPE_TIME2 => {
                metadata.len() == 1 && metadata[0] <= 6
            }
            MYSQL_TYPE_FLOAT => metadata == [4],
            MYSQL_TYPE_DOUBLE => metadata == [8],
            MYSQL_TYPE_BLOB
            | MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB => metadata.len() == 1 && (1..=4).contains(&metadata[0]),
            MYSQL_TYPE_JSON => metadata == [4],
            MYSQL_TYPE_ENUM => metadata.len() == 2 && (1..=2).contains(&metadata[1]),
            MYSQL_TYPE_SET => metadata.len() == 2 && (1..=8).contains(&metadata[1]),
            MYSQL_TYPE_BIT => {
                metadata.len() == 2
                    && metadata[0] <= 7
                    && (1..=64).contains(&(u16::from(metadata[1]) * 8 + u16::from(metadata[0])))
            }
            MYSQL_TYPE_STRING | MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => metadata.len() == 2,
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR | MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE
            | MYSQL_TYPE_TIME | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_NULL => {
                metadata.is_empty()
            }
            _ => false,
        };
        anyhow::ensure!(valid, "unsupported or malformed MySQL {kind:?} metadata");
    }
    Ok(())
}
