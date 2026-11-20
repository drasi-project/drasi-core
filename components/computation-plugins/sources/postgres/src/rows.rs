// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Row outputs: one envelope per change, each carrying a [`RowPosition`].
//!
//! A transaction is buffered until its commit while it fits within
//! `maxTransactionChanges` and `maxTransactionBytes`, so each row knows the
//! transaction's change count and the last row confirms the commit. A larger
//! transaction is streamed instead: its rows are emitted as they decode with
//! an unknown count, and the commit emits a progress record that completes
//! it. Row outputs promise no transaction atomicity, so no size stops them.
//! Progress records (no row) also mark commits without selected rows and
//! quiet WAL ranges, so committed cursors follow the WAL.

use crate::shared::SharedCounters;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use drasi_computation_contracts::{
    exchange::{SourceEvent, SourceEventWrapper},
    GraphChangeCodec, OutputEnvelope, PortId, SourceProgressSnapshot, SourceTransactionLimits,
    StreamId,
};
use drasi_core_types::models::SourceChange;
use drasi_postgres_common::{
    replication::{
        rows::{row_checkpoint, RowPosition},
        Changes, Transactions,
    },
    types::TransactionInfo,
};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::time::Instant;

/// The transaction being decoded.
struct Open {
    commit_lsn: u64,
    timestamp: DateTime<Utc>,
    /// Changes decoded so far, which is the next row's offset.
    decoded: usize,
    /// Buffered changes and their estimated size while not streaming.
    buffer: Vec<SourceChange>,
    bytes: usize,
    /// The transaction outgrew the limits; its rows stream.
    streaming: bool,
    /// Rows at or below this offset are already handled (a partial cursor
    /// replays its commit), with the cursor's known count, if any.
    handled: Option<(usize, usize)>,
    /// A handled or emitted row of this transaction did not know the count,
    /// so the commit must be completed by a progress record.
    open_rows: bool,
    deadline: Instant,
}

pub(crate) struct RowEmission {
    stream: StreamId,
    resume: Option<RowPosition>,
    sequence: u64,
    previous_lsn: u64,
    transport: Arc<AtomicU64>,
    admitted_sequence: u64,
    admitted_lsn: u64,
    shared: Option<Arc<SharedCounters>>,
    boundaries: BTreeSet<u64>,
    limits: SourceTransactionLimits,
    open: Option<Open>,
}

impl RowEmission {
    pub fn new(
        stream: StreamId,
        resume: Option<RowPosition>,
        previous_lsn: u64,
        transport: Arc<AtomicU64>,
        limits: SourceTransactionLimits,
    ) -> Self {
        Self {
            stream,
            sequence: resume.as_ref().map_or(0, |position| position.sequence),
            admitted_sequence: resume.as_ref().map_or(0, |position| position.sequence),
            admitted_lsn: resume
                .as_ref()
                .map_or(previous_lsn, |position| position.commit_lsn),
            resume,
            previous_lsn,
            transport,
            shared: None,
            boundaries: BTreeSet::new(),
            limits,
            open: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn shared(
        stream: StreamId,
        resume: Option<RowPosition>,
        previous_lsn: u64,
        transport: Arc<AtomicU64>,
        limits: SourceTransactionLimits,
        counters: Arc<SharedCounters>,
        counter_floor: u64,
        admitted_lsn: u64,
        boundaries: BTreeSet<u64>,
    ) -> Self {
        let mut result = Self::new(stream, resume, previous_lsn, transport, limits);
        result.sequence = counters.raise(&result.transport, counter_floor, admitted_lsn);
        result.admitted_sequence = result.admitted_sequence.max(result.sequence);
        result.shared = Some(counters);
        result.boundaries = boundaries;
        result
    }

    pub fn confirmed(
        &self,
        progress: &SourceProgressSnapshot,
        transaction: &Transactions,
    ) -> Result<Option<u64>> {
        row_checkpoint(progress, &transaction.source, transaction.binding)?
            .map(|position| {
                anyhow::ensure!(
                    position.sequence <= self.admitted_sequence
                        && position.commit_lsn <= self.admitted_lsn,
                    "PostgreSQL row cursor moved beyond admitted rows"
                );
                Ok(position.feedback_lsn())
            })
            .transpose()
    }

    /// Whether a pause may take effect now: between transactions, or while
    /// a transaction streams (its rows carry partial cursors). A buffered
    /// transaction is finished first.
    pub fn at_boundary(&self) -> bool {
        self.open.as_ref().is_none_or(|open| open.streaming)
    }

    /// The deadline of a buffered transaction. A streaming transaction has
    /// none: its rows are already flowing.
    pub fn deadline(&self) -> Option<Instant> {
        self.open
            .as_ref()
            .filter(|open| !open.streaming)
            .map(|open| open.deadline)
    }

    pub fn begin(&mut self, info: &TransactionInfo) -> Result<()> {
        anyhow::ensure!(self.open.is_none(), "PostgreSQL row transaction overlaps");
        let handled = match &self.resume {
            Some(position) if !position.complete() => {
                anyhow::ensure!(
                    position.commit_lsn == info.commit_lsn,
                    "retained PostgreSQL partial transaction changed or is missing"
                );
                Some((
                    position.offset.context("missing PostgreSQL row offset")?,
                    position.count,
                ))
            }
            _ => None,
        };
        // A later subscriber snapshot can lie between visible commits. Carry
        // that real coverage boundary into the next transaction's partial rows.
        if let Some(boundary) = self.boundaries.range(..info.commit_lsn).next_back() {
            self.previous_lsn = self.previous_lsn.max(*boundary);
        }
        self.open = Some(Open {
            commit_lsn: info.commit_lsn,
            timestamp: info.commit_timestamp,
            decoded: 0,
            buffer: Vec::new(),
            bytes: 0,
            streaming: false,
            open_rows: handled.is_some_and(|(_, count)| count == 0),
            handled,
            deadline: Instant::now()
                .checked_add(self.limits.duration())
                .context("PostgreSQL transaction deadline overflow")?,
        });
        Ok(())
    }

    /// Takes one message's changes. Buffered changes wait for the commit; a
    /// transaction that outgrows the limits starts streaming here.
    pub fn push(
        &mut self,
        changes: Changes,
        transaction: &Transactions,
        output: &mut VecDeque<OutputEnvelope>,
    ) -> Result<()> {
        let open = self
            .open
            .as_mut()
            .context("PostgreSQL row outside transaction")?;
        if !open.streaming {
            open.bytes = open.bytes.saturating_add(changes.bytes);
            open.buffer.extend(changes);
            if open.buffer.len() <= self.limits.max_changes.get()
                && open.bytes <= self.limits.max_bytes.get()
            {
                return Ok(());
            }
            log::info!(
                "PostgreSQL transaction {:X} exceeds the buffering limits; streaming its rows",
                open.commit_lsn
            );
            open.streaming = true;
            let buffered = std::mem::take(&mut open.buffer);
            for change in buffered {
                self.emit_row(change, None, transaction, output)?;
            }
            return Ok(());
        }
        for change in changes {
            self.emit_row(change, None, transaction, output)?;
        }
        Ok(())
    }

    /// Completes the open transaction. A commit without selected rows emits
    /// a progress record only when `idle` asks for one.
    pub fn commit(
        &mut self,
        info: &TransactionInfo,
        idle: bool,
        transaction: &Transactions,
        output: &mut VecDeque<OutputEnvelope>,
    ) -> Result<()> {
        let open = self
            .open
            .as_mut()
            .context("PostgreSQL commit without rows")?;
        anyhow::ensure!(
            open.commit_lsn == info.commit_lsn,
            "PostgreSQL commit does not match its rows"
        );
        let count = open.decoded + open.buffer.len();
        if let Some((offset, known)) = open.handled {
            anyhow::ensure!(
                offset < count && (known == 0 || known == count),
                "retained PostgreSQL partial transaction changed or is missing"
            );
        }
        let buffered = std::mem::take(&mut open.buffer);
        let streamed = open.streaming || open.open_rows;
        for change in buffered {
            self.emit_row(change, Some(count), transaction, output)?;
        }
        if streamed || (count == 0 && idle) {
            output.push_back(self.progress_record(
                info.commit_lsn,
                info.commit_timestamp,
                transaction,
            )?);
        }
        self.open = None;
        self.admit(info.commit_lsn);
        Ok(())
    }

    /// A progress record at `lsn`, a boundary holding no unhandled change.
    pub fn progress(
        &mut self,
        lsn: u64,
        timestamp: DateTime<Utc>,
        transaction: &Transactions,
    ) -> Result<OutputEnvelope> {
        anyhow::ensure!(
            self.open.is_none(),
            "PostgreSQL progress inside a transaction"
        );
        let record = self.progress_record(lsn, timestamp, transaction)?;
        self.admit(lsn);
        Ok(record)
    }

    /// A record of `position` with a fresh sequence: a fresh member of a
    /// group commits it to join at that position.
    pub fn join(
        &mut self,
        position: Option<&RowPosition>,
        origin: u64,
        transaction: &Transactions,
    ) -> Result<OutputEnvelope> {
        let sequence = self.record_sequence()?;
        let position = match position {
            Some(position) if !position.complete() => RowPosition {
                sequence,
                ..position.clone()
            },
            Some(position) => RowPosition::progress(
                transaction.binding,
                position.commit_lsn,
                sequence,
                transaction.managed_slot.clone(),
            ),
            None => RowPosition::progress(
                transaction.binding,
                origin,
                sequence,
                transaction.managed_slot.clone(),
            ),
        };
        self.admitted_sequence = self.admitted_sequence.max(sequence);
        self.encode(position, None, Utc::now(), transaction)
    }

    /// One sequence for a progress record, which has no separate transport
    /// sequence: the next logical sequence, raised to the next transport
    /// sequence so both stay increasing.
    fn record_sequence(&mut self) -> Result<u64> {
        let logical = self.next_sequence()?;
        let previous = self
            .transport
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1).map(|next| next.max(logical))
            })
            .map_err(|_| anyhow::anyhow!("PostgreSQL row transport sequence exhausted"))?;
        let sequence = previous
            .checked_add(1)
            .context("PostgreSQL row transport sequence exhausted")?
            .max(logical);
        self.sequence = sequence;
        if let Some(counters) = &self.shared {
            counters.logical.fetch_max(sequence, Ordering::Relaxed);
        }
        Ok(sequence)
    }

    fn admit(&mut self, lsn: u64) {
        self.previous_lsn = self.previous_lsn.max(lsn);
        self.admitted_sequence = self.admitted_sequence.max(self.sequence);
        self.admitted_lsn = self.admitted_lsn.max(lsn);
        if let Some(counters) = &self.shared {
            counters.admitted_lsn.fetch_max(lsn, Ordering::Relaxed);
        }
        self.resume = None;
        self.boundaries.retain(|boundary| *boundary > lsn);
    }

    fn progress_record(
        &mut self,
        lsn: u64,
        timestamp: DateTime<Utc>,
        transaction: &Transactions,
    ) -> Result<OutputEnvelope> {
        let sequence = self.record_sequence()?;
        let position = RowPosition::progress(
            transaction.binding,
            lsn,
            sequence,
            transaction.managed_slot.clone(),
        );
        self.encode(position, None, timestamp, transaction)
    }

    /// Emits the next row of the open transaction, unless a partial cursor
    /// already handled it. `count` is `None` while streaming.
    fn emit_row(
        &mut self,
        change: SourceChange,
        count: Option<usize>,
        transaction: &Transactions,
        output: &mut VecDeque<OutputEnvelope>,
    ) -> Result<()> {
        let open = self
            .open
            .as_mut()
            .context("PostgreSQL row outside transaction")?;
        let offset = open.decoded;
        open.decoded += 1;
        if open.handled.is_some_and(|(handled, _)| offset <= handled) {
            return Ok(());
        }
        open.open_rows |= count.is_none();
        let (commit_lsn, timestamp) = (open.commit_lsn, open.timestamp);
        let sequence = self.next_sequence()?;
        let position = RowPosition {
            version: 1,
            binding: transaction.binding,
            commit_lsn,
            previous_lsn: self.previous_lsn,
            offset: Some(offset),
            count: count.unwrap_or(0),
            sequence,
            managed_slot: transaction.managed_slot.clone(),
        };
        // An emitted row may be committed before its transaction completes
        // (a streamed transaction), so it is admitted now.
        self.admitted_sequence = self.admitted_sequence.max(sequence);
        self.admitted_lsn = self.admitted_lsn.max(commit_lsn);
        if let Some(counters) = &self.shared {
            counters
                .admitted_lsn
                .fetch_max(commit_lsn, Ordering::Relaxed);
        }
        output.push_back(self.encode(position, Some(change), timestamp, transaction)?);
        Ok(())
    }

    fn next_sequence(&mut self) -> Result<u64> {
        self.sequence = match &self.shared {
            Some(counters) => {
                counters.raise(&self.transport, self.sequence, self.admitted_lsn);
                counters.allocate(&self.transport)?
            }
            None => self
                .sequence
                .checked_add(1)
                .context("PostgreSQL row sequence exhausted")?,
        };
        Ok(self.sequence)
    }

    fn encode(
        &self,
        position: RowPosition,
        change: Option<SourceChange>,
        timestamp: DateTime<Utc>,
        transaction: &Transactions,
    ) -> Result<OutputEnvelope> {
        position.validate()?;
        let stream = self.stream.clone();
        let bytes = serde_json::to_vec(&position)?;
        let envelope = match change {
            Some(change) => GraphChangeCodec::encode_source_event(
                Arc::new(
                    SourceEventWrapper::new(
                        transaction.source.clone(),
                        SourceEvent::Change(change),
                        timestamp,
                        position.sequence,
                    )
                    .with_source_position(bytes.into()),
                ),
                &transaction.source,
                stream,
                self.next_transport()?,
                None,
            )?,
            // A record's sequence is both its logical and transport sequence.
            None => GraphChangeCodec::encode_source_progress(
                &transaction.source,
                stream,
                position.sequence,
                bytes.into(),
                timestamp,
            )?,
        };
        Ok(OutputEnvelope {
            execution_context: drasi_computation_contracts::telemetry::ExecutionContext::current(),
            port: PortId::try_new("out")?,
            envelope,
        })
    }

    fn next_transport(&self) -> Result<u64> {
        Ok(self
            .transport
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("PostgreSQL row transport sequence exhausted"))?
            + 1)
    }

    pub fn admitted_lsn(&self) -> u64 {
        self.shared.as_ref().map_or(self.admitted_lsn, |counters| {
            counters.admitted_lsn.load(Ordering::Relaxed)
        })
    }

    pub fn admitted_sequence(&self) -> u64 {
        self.shared
            .as_ref()
            .map_or(self.admitted_sequence, |counters| {
                counters.logical.load(Ordering::Relaxed)
            })
    }

    pub fn validate_shared_observation(&self, counter: u64, lsn: u64) -> Result<()> {
        anyhow::ensure!(
            self.shared.is_some()
                && counter <= self.admitted_sequence()
                && lsn <= self.admitted_lsn(),
            "PostgreSQL shared cursor moved beyond admitted rows"
        );
        Ok(())
    }

    pub fn rewind(&mut self, position: Option<RowPosition>, previous_lsn: u64) {
        self.sequence = position.as_ref().map_or(0, |position| position.sequence);
        self.resume = position;
        self.previous_lsn = previous_lsn;
        self.open = None;
    }

    pub fn rewind_shared(
        &mut self,
        position: Option<RowPosition>,
        previous_lsn: u64,
        counter_floor: u64,
        admitted_lsn: u64,
        boundaries: BTreeSet<u64>,
    ) -> Result<()> {
        let counters = self
            .shared
            .as_ref()
            .context("PostgreSQL shared row counters are not bound")?;
        self.sequence = counters.raise(&self.transport, counter_floor, admitted_lsn);
        self.admitted_sequence = self.admitted_sequence.max(self.sequence);
        self.resume = position;
        self.previous_lsn = previous_lsn;
        self.boundaries = boundaries;
        self.open = None;
        Ok(())
    }
}
