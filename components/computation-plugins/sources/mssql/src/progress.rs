// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The fixed consumer set: the minimum committed cursor, and the checks
//! that fence a failed, reset or rebound member.
//!
//! A member that is momentarily not ready (a consumer restarting) does not
//! fail the source (MS-11): the worker holds polling and emission until
//! every member is ready again. Failure, reset, regression, a cursor under
//! another binding and a membership change remain fatal.

use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::{
    ComponentId, ConsumerProgressSnapshot, SourceProgressKey, StreamId,
};
use drasi_mssql_common::native::Cursor;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

/// What one observation of the consumer set found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observation {
    /// Every member is ready.
    pub ready: bool,
    /// The minimum committed cursor, if any member has one.
    pub minimum: Option<Cursor>,
    /// Members without a committed cursor.
    pub missing: usize,
    /// The largest logical or transport sequence any member has seen.
    pub sequence: u64,
}

/// Tracks the consumer set across one activation.
pub(crate) struct Membership {
    source: ComponentId,
    stream: StreamId,
    members: BTreeSet<ComponentId>,
    observed: BTreeMap<ComponentId, (u64, Option<Cursor>)>,
}

impl Membership {
    pub fn new(source: ComponentId, stream: StreamId, members: BTreeSet<ComponentId>) -> Self {
        Self {
            source,
            stream,
            members,
            observed: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Forgets per-activation observations (at stop).
    pub fn reset(&mut self) {
        self.observed.clear();
    }

    pub fn observe(&mut self, snapshot: &ConsumerProgressSnapshot) -> Result<Observation> {
        ensure!(
            snapshot.consumers.keys().eq(self.members.iter()),
            "SQL Server consumer membership changed; the host must replace the source"
        );
        let mut minimum: Option<Cursor> = None;
        let mut missing = 0;
        let mut ready = true;
        let mut sequence = 0u64;
        let key = SourceProgressKey::Source(self.source.clone());
        for (id, state) in &snapshot.consumers {
            if let Some(failure) = &state.failure {
                anyhow::bail!("SQL Server consumer {id} failed: {failure}");
            }
            ready &= state.ready;
            let checkpoint = state.checkpoints.get(&key);
            let cursor = checkpoint
                .map(|checkpoint| {
                    Cursor::decode(
                        checkpoint
                            .source_position
                            .as_deref()
                            .context("SQL Server checkpoint has no CDC position")?,
                    )
                })
                .transpose()?;
            if let Some((generation, previous)) = self.observed.get(id) {
                ensure!(
                    *generation == state.reset_generation,
                    "SQL Server consumer {id} was reset during the activation; the source must be rebuilt"
                );
                if let Some(previous) = previous {
                    let current = cursor
                        .as_ref()
                        .with_context(|| format!("SQL Server consumer {id} lost its cursor"))?;
                    ensure!(
                        current.compare(previous)? != Ordering::Less,
                        "SQL Server consumer {id} cursor regressed"
                    );
                }
            }
            self.observed
                .insert(id.clone(), (state.reset_generation, cursor.clone()));
            sequence = sequence
                .max(checkpoint.map_or(0, |checkpoint| checkpoint.sequence))
                .max(
                    state
                        .transport_sequences
                        .get(&self.stream)
                        .copied()
                        .unwrap_or(0),
                );
            match cursor {
                Some(cursor) => {
                    if minimum
                        .as_ref()
                        .map(|min| cursor.compare(min))
                        .transpose()?
                        .is_none_or(|order| order == Ordering::Less)
                    {
                        minimum = Some(cursor);
                    }
                }
                None => missing += 1,
            }
        }
        Ok(Observation {
            ready,
            minimum,
            missing,
            sequence,
        })
    }
}
