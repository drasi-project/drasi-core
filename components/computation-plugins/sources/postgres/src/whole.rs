// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Whole-transaction outputs: each commit is assembled within
//! `maxTransactionChanges`, `maxTransactionBytes` and
//! `maxTransactionDurationMs` and emitted as one source transaction, never a
//! prefix. A commit beyond a limit fails the source with a typed
//! `SourceTransactionError::Limit` before anything of it is published or
//! acknowledged; raising the limit replays it whole. Empty committed
//! transactions are the progress records of these outputs.

use crate::shared::TransactionEmission;
use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_computation_contracts::{
    ComponentId, OutputEnvelope, PortId, SourceTransactionBuilder, SourceTransactionLimits,
    StreamId,
};
use drasi_postgres_common::replication::{Changes, Position};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::time::Instant;

/// How a whole transaction's cursor and logical sequence are chosen.
pub(crate) enum Cursors {
    /// Query-owned progress: the commit LSN is the logical sequence.
    Single,
    /// A fixed consumer set: a shared logical counter.
    Shared(TransactionEmission),
}

pub(crate) struct WholeEmission {
    source: ComponentId,
    stream: StreamId,
    limits: SourceTransactionLimits,
    transport: Arc<AtomicU64>,
    cursors: Cursors,
    /// The open transaction's assembly and its change count.
    open: Option<(SourceTransactionBuilder, usize)>,
}

impl WholeEmission {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        limits: SourceTransactionLimits,
        transport: Arc<AtomicU64>,
        cursors: Cursors,
    ) -> Self {
        Self {
            source,
            stream,
            limits,
            transport,
            cursors,
            open: None,
        }
    }

    pub fn shared(&self) -> Option<&TransactionEmission> {
        match &self.cursors {
            Cursors::Shared(emission) => Some(emission),
            Cursors::Single => None,
        }
    }

    pub fn at_boundary(&self) -> bool {
        self.open.is_none()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.open.as_ref().map(|(builder, _)| builder.deadline())
    }

    pub fn check_deadline(&mut self) -> Result<()> {
        if let Some((builder, _)) = &mut self.open {
            builder.check_deadline()?;
        }
        Ok(())
    }

    pub fn begin(&mut self) -> Result<()> {
        anyhow::ensure!(self.open.is_none(), "incomplete PostgreSQL transaction");
        self.open = Some((SourceTransactionBuilder::new(&self.source, self.limits)?, 0));
        Ok(())
    }

    pub fn push(&mut self, changes: Changes) -> Result<()> {
        let (builder, count) = self
            .open
            .as_mut()
            .context("PostgreSQL change outside transaction")?;
        for change in changes {
            builder.push(change)?;
            *count += 1;
        }
        Ok(())
    }

    /// Completes the open transaction. A transaction without selected
    /// changes is emitted only when `idle` asks for a progress record.
    pub fn commit(
        &mut self,
        commit_lsn: u64,
        timestamp: DateTime<Utc>,
        idle: bool,
        binding: [u8; 32],
        managed_slot: Option<&str>,
    ) -> Result<Option<OutputEnvelope>> {
        let (builder, count) = self.open.take().context("Commit without assembly")?;
        if count == 0 && !idle {
            self.admit(commit_lsn);
            return Ok(None);
        }
        self.emit(builder, commit_lsn, timestamp, binding, managed_slot)
            .map(Some)
    }

    /// An empty committed transaction at `lsn`, a boundary holding no
    /// unhandled change: the progress record of a whole-transaction output.
    pub fn progress(
        &mut self,
        lsn: u64,
        timestamp: DateTime<Utc>,
        binding: [u8; 32],
        managed_slot: Option<&str>,
    ) -> Result<OutputEnvelope> {
        anyhow::ensure!(
            self.open.is_none(),
            "PostgreSQL progress inside a transaction"
        );
        let builder = SourceTransactionBuilder::new(&self.source, self.limits)?;
        self.emit(builder, lsn, timestamp, binding, managed_slot)
    }

    fn emit(
        &mut self,
        builder: SourceTransactionBuilder,
        lsn: u64,
        timestamp: DateTime<Utc>,
        binding: [u8; 32],
        managed_slot: Option<&str>,
    ) -> Result<OutputEnvelope> {
        let (position, logical) = match &self.cursors {
            Cursors::Single => (
                serde_json::to_vec(&Position {
                    version: 1,
                    binding,
                    commit_lsn: lsn,
                    managed_slot: managed_slot.map(str::to_owned),
                })?,
                lsn,
            ),
            Cursors::Shared(emission) => emission.position(lsn, binding, managed_slot)?,
        };
        let transaction = builder.commit(
            Bytes::copy_from_slice(&lsn.to_be_bytes()),
            Bytes::from(position),
        )?;
        let transport = self
            .transport
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("PostgreSQL transport sequence exhausted"))?
            + 1;
        let envelope =
            transaction.into_replay_envelope(self.stream.clone(), logical, transport, timestamp)?;
        self.admit(lsn);
        Ok(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        })
    }

    fn admit(&self, lsn: u64) {
        if let Cursors::Shared(emission) = &self.cursors {
            emission.admit(lsn);
        }
    }

    /// The highest LSN a committed cursor may name: the decoder's handled
    /// position for query-owned progress, the shared admission bound
    /// otherwise.
    pub fn admitted_lsn(&self, handled: u64) -> u64 {
        match &self.cursors {
            Cursors::Single => handled,
            Cursors::Shared(emission) => emission.admitted_lsn(),
        }
    }

    pub fn rewind(&mut self) {
        self.open = None;
    }
}
