// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The destination cursor: its format, owner, table names and the pure
//! decision for one operation.
//!
//! One row per `(owner, stream_key)` holds the stream's current batch
//! (`sequence`, `digest`, `operations`), its completed operation prefix
//! (`completed`), the at-least-once producer `epoch` (empty for exactly-once
//! streams), the durable query output `generation` (0 when the producer has
//! none) and `pending`: the operation whose effect is running in an open
//! delivery transaction, or -1. `pending` is written in the same transaction
//! as the effect and cleared by the cursor advance, so it is durable only
//! when the effect escaped that transaction (the command committed it
//! itself); a later attempt of that operation then refuses to run it again.

use drasi_computation_contracts::{
    delivery::{cursor_epoch, valid_cursor_epoch, CursorEpoch},
    DeliveryItem,
};
use sha2::{Digest, Sha256};
use std::fmt;

/// The cursor row format written by every stored-procedure sink.
pub const PROGRESS_FORMAT_VERSION: i16 = 4;
/// `pending` when no effect is in flight.
pub const NO_PENDING: i64 = -1;
/// Domain separator of the owner digest.
pub const OWNER_DOMAIN: &[u8] = b"drasi.storedproc/owner/v1\0";
/// The default progress table, in the backend's default schema.
pub const DEFAULT_PROGRESS_TABLE: &str = "drasi_delivery_progress";
/// Suffix of the dead-letter table that records skipped operations.
pub const DEAD_LETTER_SUFFIX: &str = "_dead_letter";
/// The longest identifier every backend accepts (PostgreSQL's 63 bytes).
pub const MAX_IDENTIFIER_BYTES: usize = 63;
/// The default number of streams one owner may keep.
pub const DEFAULT_MAX_STREAMS: u32 = 4096;
/// The largest configurable number of streams per owner.
pub const MAX_STREAMS_LIMIT: u32 = 1_000_000;
/// The longest recorded dead-letter error text, in characters.
pub const MAX_ERROR_CHARS: usize = 4000;

/// The cursor owner of one sink: lowercase hex SHA-256 of
/// `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id`. Stable
/// across restarts and configuration updates of the same component.
pub fn owner(graph_id: &str, component_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(OWNER_DOMAIN);
    digest.update(graph_id.as_bytes());
    digest.update([0]);
    digest.update(component_id.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Whether `owner` has the owner digest's shape.
pub fn valid_owner(owner: &str) -> bool {
    owner.len() == 64
        && owner
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// A validated progress table name: `table` or `schema.table`, each part a
/// lowercase identifier (`[a-z_][a-z0-9_]*`) short enough that the
/// dead-letter table name (`<table>_dead_letter`) also fits every backend.
/// An unqualified name lives in the backend's default schema (PostgreSQL
/// `public`, MySQL the configured `database`, SQL Server `dbo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressTable {
    schema: Option<String>,
    table: String,
}

impl ProgressTable {
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        let (schema, table) = match name.split_once('.') {
            Some((schema, table)) => (Some(schema), table),
            None => (None, name),
        };
        for (part, value, limit) in schema
            .map(|schema| ("schema", schema, MAX_IDENTIFIER_BYTES))
            .into_iter()
            .chain([(
                "table",
                table,
                MAX_IDENTIFIER_BYTES - DEAD_LETTER_SUFFIX.len(),
            )])
        {
            let mut characters = value.chars();
            anyhow::ensure!(
                !value.is_empty()
                    && value.len() <= limit
                    && characters
                        .next()
                        .is_some_and(|first| first == '_' || first.is_ascii_lowercase())
                    && characters.all(|next| {
                        next == '_' || next.is_ascii_lowercase() || next.is_ascii_digit()
                    }),
                "progressTable {part} '{value}' must be a lowercase identifier ([a-z_][a-z0-9_]*, \
                 at most {limit} bytes); progressTable is table or schema.table"
            );
        }
        Ok(Self {
            schema: schema.map(str::to_owned),
            table: table.to_owned(),
        })
    }

    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// The dead-letter table, in the same schema.
    pub fn dead_letter(&self) -> Self {
        Self {
            schema: self.schema.clone(),
            table: format!("{}{DEAD_LETTER_SUFFIX}", self.table),
        }
    }

    /// The name with `default_schema` filled in, quoted with `quote`.
    pub fn quoted(&self, default_schema: &str, quote: impl Fn(&str) -> String) -> String {
        format!(
            "{}.{}",
            quote(self.schema.as_deref().unwrap_or(default_schema)),
            quote(&self.table)
        )
    }
}

impl fmt::Display for ProgressTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(schema) => write!(f, "{schema}.{}", self.table),
            None => f.write_str(&self.table),
        }
    }
}

/// A deterministic rejection by the cursor. Retrying the same operation gives
/// the same answer, and the operation cannot be settled as skipped either:
/// the cursor itself disagrees with it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CursorError {
    #[error(
        "destination delivery receipt expired: batch {sequence} is older than the cursor's \
         batch {cursor}"
    )]
    ReceiptExpired { sequence: u64, cursor: u64 },
    #[error("destination delivery payload conflicts with its receipt")]
    PayloadConflict,
    #[error("destination has unfinished operations or the batch is missing its prefix")]
    UnfinishedBatch,
    #[error(
        "destination delivery has an operation gap: expected operation {expected}, got {actual}"
    )]
    OperationGap { expected: i64, actual: i64 },
    #[error("destination delivery is missing its first operation")]
    MissingFirstOperation,
    #[error("destination delivery cursor belongs to a stream with a different delivery guarantee")]
    GuaranteeMismatch,
    #[error(
        "destination delivery belongs to query output generation {generation}, superseded by \
         the cursor's generation {cursor}"
    )]
    SupersededGeneration { generation: i64, cursor: i64 },
    #[error("invalid destination delivery progress: {0}")]
    InvalidProgress(&'static str),
    #[error(
        "destination delivery stream capacity ({max}) is exhausted for this sink; raise \
         maxStreams or erase stale streams"
    )]
    StreamCapacity { max: u32 },
    #[error("destination delivery cursor changed during its effect")]
    CursorChanged,
}

/// A raw cursor row, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorRow {
    pub version: i16,
    pub sequence: String,
    pub digest: Vec<u8>,
    pub operations: i64,
    pub completed: i64,
    /// Empty for exactly-once streams.
    pub epoch: String,
    /// 0 when the producer has no output generation.
    pub generation: i64,
    /// [`NO_PENDING`] unless an effect escaped its delivery transaction.
    pub pending: i64,
}

struct Cursor {
    sequence: u64,
    completed: i64,
}

impl CursorRow {
    fn parse(&self, position: &Position) -> Result<Cursor, CursorError> {
        if self.version != PROGRESS_FORMAT_VERSION {
            return Err(CursorError::InvalidProgress(
                "unsupported format version (earlier formats are not migrated)",
            ));
        }
        let sequence = self
            .sequence
            .parse::<u64>()
            .ok()
            .filter(|sequence| *sequence > 0 && sequence.to_string() == self.sequence)
            .ok_or(CursorError::InvalidProgress(
                "sequence is not a positive decimal",
            ))?;
        if self.digest.len() != 32
            || self.operations <= 0
            || self.completed < 0
            || self.completed > self.operations
            || self.generation < 0
            || !(self.pending == NO_PENDING || self.pending == self.completed)
            || self.pending >= self.operations
            || !valid_cursor_epoch(&self.epoch)
        {
            return Err(CursorError::InvalidProgress(
                "digest, operations, completed, generation, pending or epoch out of range",
            ));
        }
        // The digest and operation count matter only for the same batch.
        if sequence == position.sequence
            && self.generation == position.generation
            && self.epoch == position.epoch
            && (self.digest.as_slice() != position.digest.as_slice()
                || self.operations != position.operations)
        {
            return Err(CursorError::PayloadConflict);
        }
        Ok(Cursor {
            sequence,
            completed: self.completed,
        })
    }
}

/// The ordered position of one operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub stream: String,
    pub sequence: u64,
    pub operation: i64,
    pub operations: i64,
    pub digest: [u8; 32],
    /// Empty for exactly-once delivery; otherwise the producer epoch.
    pub epoch: String,
    /// The durable query producer's output generation; 0 when it has none.
    pub generation: i64,
    /// The host's idempotency key of the operation.
    pub delivery_id: String,
}

impl Position {
    pub fn from_item(item: &DeliveryItem<'_>) -> anyhow::Result<Self> {
        item.position.validate()?;
        Ok(Self {
            stream: item.position.stream().to_owned(),
            sequence: item.position.sequence(),
            operation: i64::try_from(item.position.operation())?,
            operations: i64::try_from(item.position.operations())?,
            digest: item.content_digest,
            epoch: item.position.epoch().unwrap_or_default().to_owned(),
            generation: i64::try_from(item.position.generation().unwrap_or_default())?,
            delivery_id: item.id.as_str().to_owned(),
        })
    }
}

/// What the cursor says about an operation.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Already settled (applied or skipped): nothing to do.
    Duplicate,
    /// Apply it after writing the cursor as stated, then advance from
    /// `completed`. `escaped` means an earlier attempt's effect escaped its
    /// transaction, so the effect must not run again.
    Apply {
        completed: i64,
        write: CursorWrite,
        escaped: bool,
    },
}

/// The cursor write that precedes the effect.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CursorWrite {
    /// The batch is the cursor's batch.
    Keep,
    /// The stream's first batch: insert a row (counts against `maxStreams`).
    Insert,
    /// A new batch after a completed one, the first batch of a new producer
    /// epoch, or the first batch of a newer query output generation: replace
    /// the row's batch.
    Rollover,
}

fn fresh(position: &Position, write: CursorWrite) -> Result<Decision, CursorError> {
    if position.operation != 0 {
        return Err(CursorError::MissingFirstOperation);
    }
    Ok(Decision::Apply {
        completed: 0,
        write,
        escaped: false,
    })
}

/// The pure cursor decision for one operation.
pub fn decide(previous: Option<&CursorRow>, position: &Position) -> Result<Decision, CursorError> {
    let Some(previous) = previous else {
        return fresh(position, CursorWrite::Insert);
    };
    let cursor = previous.parse(position)?;
    match cursor_epoch(&previous.epoch, &position.epoch) {
        Err(_) => return Err(CursorError::GuaranteeMismatch),
        // A new producer run: the previous epoch's row is absent for it.
        Ok(CursorEpoch::New) => return fresh(position, CursorWrite::Rollover),
        Ok(CursorEpoch::Same) => {}
    }
    match position.generation.cmp(&previous.generation) {
        std::cmp::Ordering::Less => {
            return Err(CursorError::SupersededGeneration {
                generation: position.generation,
                cursor: previous.generation,
            })
        }
        // A reset or recreated query: its new sequence space replaces the row.
        std::cmp::Ordering::Greater => return fresh(position, CursorWrite::Rollover),
        std::cmp::Ordering::Equal => {}
    }
    if position.sequence < cursor.sequence {
        return Err(CursorError::ReceiptExpired {
            sequence: position.sequence,
            cursor: cursor.sequence,
        });
    }
    if position.sequence == cursor.sequence {
        if position.operation < cursor.completed {
            return Ok(Decision::Duplicate);
        }
        if position.operation != cursor.completed {
            return Err(CursorError::OperationGap {
                expected: cursor.completed,
                actual: position.operation,
            });
        }
        return Ok(Decision::Apply {
            completed: cursor.completed,
            write: CursorWrite::Keep,
            escaped: previous.pending == cursor.completed,
        });
    }
    if cursor.completed != previous.operations {
        return Err(CursorError::UnfinishedBatch);
    }
    fresh(position, CursorWrite::Rollover).map_err(|_| CursorError::UnfinishedBatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(sequence: u64, operation: i64) -> Position {
        Position {
            stream: "s".into(),
            sequence,
            operation,
            operations: 2,
            digest: [1; 32],
            epoch: String::new(),
            generation: 0,
            delivery_id: "d".into(),
        }
    }

    fn row(sequence: u64, completed: i64) -> CursorRow {
        CursorRow {
            version: PROGRESS_FORMAT_VERSION,
            sequence: sequence.to_string(),
            digest: vec![1; 32],
            operations: 2,
            completed,
            epoch: String::new(),
            generation: 0,
            pending: NO_PENDING,
        }
    }

    #[test]
    fn owner_is_the_documented_digest_and_tables_leave_room_for_dead_letters() {
        let expected = format!(
            "{:x}",
            Sha256::digest(b"drasi.storedproc/owner/v1\0graph\0component")
        );
        assert_eq!(owner("graph", "component"), expected);
        assert!(valid_owner(&expected));
        assert_ne!(owner("graphc", "omponent"), expected);
        let table = ProgressTable::parse("app.progress").unwrap();
        assert_eq!(table.dead_letter().to_string(), "app.progress_dead_letter");
        assert_eq!(
            ProgressTable::parse("progress")
                .unwrap()
                .quoted("dbo", |part| format!("[{part}]")),
            "[dbo].[progress]"
        );
        for bad in ["", "A", "a b", "a.b.c", "1a", "a;b", &"x".repeat(52)] {
            assert!(ProgressTable::parse(bad).is_err(), "{bad}");
        }
        assert!(ProgressTable::parse(&"x".repeat(51)).is_ok());
    }

    #[test]
    fn decisions_follow_the_protocol() {
        use CursorWrite::*;
        let apply = |completed, write| Decision::Apply {
            completed,
            write,
            escaped: false,
        };
        assert_eq!(decide(None, &position(1, 0)), Ok(apply(0, Insert)));
        assert_eq!(
            decide(None, &position(1, 1)),
            Err(CursorError::MissingFirstOperation)
        );
        assert_eq!(
            decide(Some(&row(1, 1)), &position(1, 0)),
            Ok(Decision::Duplicate)
        );
        assert_eq!(
            decide(Some(&row(1, 1)), &position(1, 1)),
            Ok(apply(1, Keep))
        );
        assert_eq!(
            decide(Some(&row(1, 2)), &position(2, 0)),
            Ok(apply(0, Rollover))
        );
        assert_eq!(
            decide(Some(&row(1, 1)), &position(2, 0)),
            Err(CursorError::UnfinishedBatch)
        );
        assert_eq!(
            decide(Some(&row(2, 2)), &position(1, 0)),
            Err(CursorError::ReceiptExpired {
                sequence: 1,
                cursor: 2
            })
        );
        let mut escaped = row(1, 1);
        escaped.pending = 1;
        assert_eq!(
            decide(Some(&escaped), &position(1, 1)),
            Ok(Decision::Apply {
                completed: 1,
                write: Keep,
                escaped: true
            })
        );
        assert_eq!(
            decide(Some(&escaped), &position(1, 0)),
            Ok(Decision::Duplicate)
        );
        let mut newer = position(1, 0);
        newer.generation = 3;
        assert_eq!(decide(Some(&row(9, 1)), &newer), Ok(apply(0, Rollover)));
        let mut current = row(9, 2);
        current.generation = 3;
        assert_eq!(
            decide(Some(&current), &position(10, 0)),
            Err(CursorError::SupersededGeneration {
                generation: 0,
                cursor: 3
            })
        );
        let mut conflict = position(1, 0);
        conflict.digest = [2; 32];
        assert_eq!(
            decide(Some(&row(1, 1)), &conflict),
            Err(CursorError::PayloadConflict)
        );
        let mut volatile = position(1, 0);
        volatile.epoch = "a".repeat(64);
        assert_eq!(
            decide(Some(&row(1, 1)), &volatile),
            Err(CursorError::GuaranteeMismatch)
        );
        let mut invalid = row(1, 1);
        invalid.version = 3;
        assert!(matches!(
            decide(Some(&invalid), &position(1, 1)),
            Err(CursorError::InvalidProgress(_))
        ));
    }
}
