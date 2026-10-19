// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    exchange::{SourceEvent, SourceEventWrapper},
    GraphChangeCodec, OutputEnvelope, PortId, SourceProgressSnapshot, SourceTransactionCodec,
};
use drasi_postgres_common::replication::{
    rows::{row_checkpoint, RowPosition},
    Transactions,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

#[derive(Clone, Copy)]
pub struct RowSourceOptions {
    pub persistent: bool,
    pub auto_create_slot: bool,
    pub start_from_slot: bool,
    pub reconnect_attempts: u32,
    pub reconnect_delay_ms: u64,
}

impl Default for RowSourceOptions {
    fn default() -> Self {
        Self {
            persistent: false,
            auto_create_slot: true,
            start_from_slot: true,
            reconnect_attempts: 1,
            reconnect_delay_ms: 5000,
        }
    }
}

impl RowSourceOptions {
    pub(crate) fn validate(self, coordinated: bool) -> Result<()> {
        anyhow::ensure!(
            self.reconnect_attempts <= 32
                && self.reconnect_delay_ms <= 60_000
                && (self.reconnect_attempts == 0 || self.reconnect_delay_ms > 0),
            "PostgreSQL reconnect requires at most 32 attempts with a 1..=60000ms delay"
        );
        anyhow::ensure!(
            !coordinated || self.persistent,
            "coordinated PostgreSQL row bootstrap requires persistent atomic query storage"
        );
        anyhow::ensure!(
            !(self.persistent || coordinated) || (!self.auto_create_slot && !self.start_from_slot),
            "recoverable PostgreSQL rows require their retained slot/explicit initial cursor or coordinated bootstrap"
        );
        Ok(())
    }
}

pub(crate) struct RowEmission {
    resume: Option<RowPosition>,
    sequence: u64,
    previous_lsn: u64,
    transport: Arc<AtomicU64>,
    admitted_sequence: u64,
    admitted_lsn: u64,
}

impl RowEmission {
    pub fn new(resume: Option<RowPosition>, previous_lsn: u64, transport: Arc<AtomicU64>) -> Self {
        Self {
            sequence: resume.as_ref().map_or(0, |position| position.sequence),
            admitted_sequence: resume.as_ref().map_or(0, |position| position.sequence),
            admitted_lsn: resume
                .as_ref()
                .map_or(previous_lsn, |position| position.commit_lsn),
            resume,
            previous_lsn,
            transport,
        }
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

    pub fn expand(
        &mut self,
        group: OutputEnvelope,
        transaction: &Transactions,
    ) -> Result<VecDeque<OutputEnvelope>> {
        let changes =
            SourceTransactionCodec::decode(&group.envelope, transaction.limits)?.into_changes();
        let count = changes.len();
        let commit_lsn = transaction.resume;
        let timestamp = group
            .envelope
            .system()
            .timestamp()
            .context("PostgreSQL transaction timestamp missing")?;
        let skip = match &self.resume {
            Some(position) if !position.complete() => {
                anyhow::ensure!(
                    position.commit_lsn == commit_lsn && position.count == count,
                    "retained PostgreSQL partial transaction changed or is missing"
                );
                position.offset.context("missing PostgreSQL row offset")? + 1
            }
            _ => 0,
        };
        let mut output = VecDeque::new();
        for (offset, change) in changes.into_iter().enumerate().skip(skip) {
            self.sequence = self
                .sequence
                .checked_add(1)
                .context("PostgreSQL row sequence exhausted")?;
            let position = RowPosition {
                version: 1,
                binding: transaction.binding,
                commit_lsn,
                previous_lsn: self.previous_lsn,
                offset: Some(offset),
                count,
                sequence: self.sequence,
                managed_slot: transaction.managed_slot.clone(),
            };
            position.validate()?;
            let transport = self
                .transport
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| anyhow::anyhow!("PostgreSQL row transport sequence exhausted"))?
                + 1;
            let event = SourceEventWrapper::new(
                transaction.source.to_string(),
                SourceEvent::Change(change),
                timestamp,
                self.sequence,
            )
            .with_source_position(serde_json::to_vec(&position)?.into());
            output.push_back(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: GraphChangeCodec::encode_source_event(
                    Arc::new(event),
                    &transaction.source,
                    transaction.stream.clone(),
                    transport,
                    None,
                )?,
            });
        }
        if count > 0 {
            self.previous_lsn = commit_lsn;
            self.admitted_sequence = self.admitted_sequence.max(self.sequence);
            self.admitted_lsn = self.admitted_lsn.max(commit_lsn);
            self.resume = None;
        }
        Ok(output)
    }

    pub fn admitted_lsn(&self) -> u64 {
        self.admitted_lsn
    }

    pub fn rewind(&mut self, position: Option<RowPosition>, previous_lsn: u64) {
        self.sequence = position.as_ref().map_or(0, |position| position.sequence);
        self.resume = position;
        self.previous_lsn = previous_lsn;
    }
}

pub(crate) fn transient(error: &anyhow::Error) -> bool {
    use std::io::ErrorKind;
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
                | ErrorKind::BrokenPipe
                | ErrorKind::TimedOut
                | ErrorKind::UnexpectedEof
        )
    }) || error.is::<tokio::time::error::Elapsed>()
        || error
            .downcast_ref::<drasi_postgres_common::protocol::ErrorResponse>()
            .is_some_and(|error| {
                matches!(
                    error.code.as_str(),
                    "57P01" | "57P02" | "57P03" | "08000" | "08001" | "08003" | "08006" | "55006"
                )
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnection_is_limited_to_transport_failures_not_rejected_work() {
        assert!(transient(
            &std::io::Error::new(std::io::ErrorKind::ConnectionReset, "closed",).into()
        ));
        assert!(!transient(
            &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied",).into()
        ));
        assert!(!transient(
            &drasi_computation_contracts::SourceTransactionError::Deadline.into()
        ));
        assert!(!transient(&anyhow::anyhow!("changed source binding")));
        assert!(!transient(&anyhow::anyhow!("missing committed row cursor")));
    }

    #[test]
    fn reconnection_bounds_are_explicit_and_do_not_enable_weaker_persistent_starts() {
        RowSourceOptions::default().validate(false).unwrap();
        for options in [
            RowSourceOptions {
                reconnect_attempts: 33,
                ..Default::default()
            },
            RowSourceOptions {
                reconnect_delay_ms: 0,
                ..Default::default()
            },
            RowSourceOptions {
                reconnect_delay_ms: 60_001,
                ..Default::default()
            },
            RowSourceOptions {
                persistent: true,
                ..Default::default()
            },
        ] {
            assert!(options.validate(false).is_err());
        }
        RowSourceOptions {
            reconnect_attempts: 0,
            reconnect_delay_ms: 0,
            ..Default::default()
        }
        .validate(false)
        .unwrap();
    }
}
