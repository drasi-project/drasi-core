// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentId, ConsumerProgressSnapshot, ConsumerProgressUpdates, ConsumerProgressView,
    InputPositionComparator, InputPositionPolicy, SourceProgressKey, SourceProgressSnapshot,
    StreamId,
};
use drasi_computation_plugin_sdk::NativeConsumerProgress;
use drasi_postgres_common::replication::rows::{
    compare_row_positions, minimum_row_checkpoint, row_checkpoint, row_position, RowPosition,
    RowProgressMinimum,
};
use drasi_postgres_common::replication::shared_transactions::{
    self as transactions, SharedTransactionPosition, TransactionProgressMinimum,
};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc, Mutex,
    },
};

#[cfg(test)]
mod tests;

pub(crate) struct RowPositionComparator;

impl InputPositionComparator for RowPositionComparator {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> Result<bool> {
        let event: RowPosition = serde_json::from_slice(event)?;
        let handled: RowPosition = serde_json::from_slice(handled)?;
        Ok(compare_row_positions(&event, &handled)? == Ordering::Greater)
    }
}

pub(crate) struct TransactionPositionComparator;
impl InputPositionComparator for TransactionPositionComparator {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> Result<bool> {
        let event: SharedTransactionPosition = serde_json::from_slice(event)?;
        let handled: SharedTransactionPosition = serde_json::from_slice(handled)?;
        Ok(transactions::compare_positions(&event, &handled)? == Ordering::Greater)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedMode {
    Rows,
    Transactions,
}

impl SharedMode {
    fn cursor(
        &self,
        state: &SourceProgressSnapshot,
        source: &ComponentId,
        binding: Option<[u8; 32]>,
    ) -> Result<Option<bool>> {
        let (actual, snapshot) = match self {
            Self::Rows => {
                let Some(position) = row_position(state, source)? else {
                    return Ok(None);
                };
                anyhow::ensure!(
                    position.managed_slot.is_none(),
                    "shared PostgreSQL progress cannot adopt a managed snapshot slot"
                );
                (position.binding, position.is_configured_snapshot())
            }
            Self::Transactions => {
                let Some(position) = transactions::read_position(state, source)? else {
                    return Ok(None);
                };
                (position.binding, position.is_configured_snapshot())
            }
        };
        anyhow::ensure!(
            binding.is_none_or(|binding| binding == actual),
            "PostgreSQL shared source binding changed"
        );
        Ok(Some(snapshot))
    }
}

// Retain allocation bounds across ordered stop/start and reader replacement.
#[derive(Default)]
pub(crate) struct SharedCounters {
    pub logical: AtomicU64,
    pub admitted_lsn: AtomicU64,
}
impl SharedCounters {
    pub fn raise(&self, transport: &AtomicU64, counter: u64, lsn: u64) -> u64 {
        let floor = counter
            .max(self.logical.load(AtomicOrdering::Relaxed))
            .max(transport.load(AtomicOrdering::Relaxed));
        self.logical.fetch_max(floor, AtomicOrdering::Relaxed);
        transport.fetch_max(floor, AtomicOrdering::Relaxed);
        self.admitted_lsn.fetch_max(lsn, AtomicOrdering::Relaxed);
        floor
    }
    pub fn allocate(&self, transport: &AtomicU64) -> Result<u64> {
        self.raise(transport, 0, 0);
        Ok(self
            .logical
            .fetch_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("PostgreSQL shared logical sequence exhausted"))?
            + 1)
    }
}

pub(crate) struct SharedBinding {
    pub reader: ConsumerProgressView,
    pub expected: BTreeSet<ComponentId>,
    pub policy: Arc<InputPositionPolicy>,
    pub counters: Arc<SharedCounters>,
    pub mode: SharedMode,
    _native_progress: Option<NativeConsumerProgress>,
    history: Arc<Mutex<Option<SharedHistory>>>,
}

impl SharedBinding {
    pub fn new(
        source: &ComponentId,
        reader: ConsumerProgressView,
        consumers: &[ComponentId],
    ) -> Result<Self> {
        Self::with_mode(source, reader, consumers, SharedMode::Rows)
    }

    pub fn transactions(source: &ComponentId, progress: NativeConsumerProgress) -> Result<Self> {
        let mut binding = Self::with_mode(
            source,
            progress.reader(),
            progress.consumer_ids(),
            SharedMode::Transactions,
        )?;
        binding._native_progress = Some(progress);
        Ok(binding)
    }

    fn with_mode(
        source: &ComponentId,
        reader: ConsumerProgressView,
        consumers: &[ComponentId],
        mode: SharedMode,
    ) -> Result<Self> {
        let expected: BTreeSet<_> = consumers.iter().cloned().collect();
        anyhow::ensure!(
            reader.producer_id() == source
                && !expected.is_empty()
                && expected.len() <= drasi_computation_contracts::MAX_CONSUMER_PROGRESS_MEMBERS
                && expected.len() == consumers.len()
                && !expected.contains(source),
            "PostgreSQL shared source requires its actual fixed consumer binding"
        );
        let comparator: Arc<dyn InputPositionComparator> = match mode {
            SharedMode::Rows => Arc::new(RowPositionComparator),
            SharedMode::Transactions => Arc::new(TransactionPositionComparator),
        };
        let policy = Arc::new(InputPositionPolicy::new(
            SourceProgressKey::Source(source.to_string()),
            comparator,
        )?);
        Ok(Self {
            reader,
            expected,
            policy,
            counters: Arc::default(),
            mode,
            _native_progress: None,
            history: Arc::default(),
        })
    }

    pub fn retained_origin(
        &self,
        snapshot: &ConsumerProgressSnapshot,
        source: &ComponentId,
    ) -> Result<Option<u64>> {
        let mut history = self
            .history
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL consumer history poisoned"))?;
        match history.as_mut() {
            Some(history) => {
                history.restart(snapshot, source)?;
                Ok(history.fresh_lsn)
            }
            None => Ok(None),
        }
    }
}

pub(crate) struct SharedObservation {
    pub minimum: Option<RowProgressMinimum>,
    pub max_counter: u64,
    pub max_lsn: u64,
    pub boundaries: BTreeSet<u64>,
    pub missing: bool,
    pub ready: bool,
}

pub(crate) fn validate_members(
    snapshot: &ConsumerProgressSnapshot,
    expected: &BTreeSet<ComponentId>,
    persistent: bool,
) -> Result<bool> {
    anyhow::ensure!(
        !expected.is_empty() && snapshot.consumers.keys().eq(expected.iter()),
        "PostgreSQL consumer progress is missing a member or changed its fixed membership"
    );
    let mut ready = true;
    for (id, state) in &snapshot.consumers {
        if let Some(failure) = &state.failure {
            anyhow::bail!("PostgreSQL consumer {id} is fenced: {failure}");
        }
        anyhow::ensure!(
            !persistent || state.persistent,
            "PostgreSQL consumer {id} has no persistent progress"
        );
        ready &= state.ready && state.recovered && state.admitting;
    }
    Ok(ready)
}

/// Only an entirely fresh, streaming-only binding may select its initial slot
/// boundary. An existing binding can retain that origin while its first inputs
/// are pending, but cannot choose a new origin for a missing consumer.
pub(crate) fn fresh_group(
    snapshot: &ConsumerProgressSnapshot,
    expected: &BTreeSet<ComponentId>,
    source: &ComponentId,
    stream: &StreamId,
    persistent: bool,
    retained_origin: Option<u64>,
) -> Result<bool> {
    fresh_group_mode(
        snapshot,
        expected,
        source,
        stream,
        persistent,
        retained_origin,
        SharedMode::Rows,
    )
}

pub(crate) fn fresh_transactions(
    snapshot: &ConsumerProgressSnapshot,
    expected: &BTreeSet<ComponentId>,
    source: &ComponentId,
    stream: &StreamId,
    retained_origin: Option<u64>,
) -> Result<bool> {
    fresh_group_mode(
        snapshot,
        expected,
        source,
        stream,
        true,
        retained_origin,
        SharedMode::Transactions,
    )
}

fn fresh_group_mode(
    snapshot: &ConsumerProgressSnapshot,
    expected: &BTreeSet<ComponentId>,
    source: &ComponentId,
    stream: &StreamId,
    persistent: bool,
    retained_origin: Option<u64>,
    mode: SharedMode,
) -> Result<bool> {
    anyhow::ensure!(
        validate_members(snapshot, expected, persistent)?,
        "PostgreSQL consumers are not ready for initialization"
    );
    let mut present = 0;
    for state in snapshot.consumers.values() {
        if mode.cursor(state, source, None)?.is_some() {
            present += 1;
        }
    }
    anyhow::ensure!(
        present == 0 || present == expected.len() || retained_origin.is_some(),
        "PostgreSQL shared consumers have mixed missing initialization"
    );
    if present != expected.len() {
        anyhow::ensure!(
            snapshot.consumers.values().all(|state| {
                state
                    .checkpoints
                    .contains_key(&SourceProgressKey::Source(source.to_string()))
                    || (!state.bootstrap_complete
                        && !state.transport_sequences.contains_key(stream)
                        && !state
                            .checkpoints
                            .contains_key(&SourceProgressKey::Stream(stream.clone())))
            }),
            "PostgreSQL shared source has missing bootstrap or replay initialization"
        );
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) fn observe(
    snapshot: &ConsumerProgressSnapshot,
    expected: &BTreeSet<ComponentId>,
    source: &ComponentId,
    stream: &StreamId,
    binding: [u8; 32],
    persistent: bool,
) -> Result<SharedObservation> {
    let ready = validate_members(snapshot, expected, persistent)?;
    let minimum = minimum_row_checkpoint(snapshot, expected, source, binding)?;
    let mut max_counter = 0;
    let mut max_lsn = 0;
    let mut boundaries = BTreeSet::new();
    let mut missing = false;
    for state in snapshot.consumers.values() {
        max_counter = max_counter
            .max(state.transport_sequences.get(stream).copied().unwrap_or(0))
            .max(
                state
                    .checkpoints
                    .get(&SourceProgressKey::Stream(stream.clone()))
                    .map_or(0, |checkpoint| checkpoint.sequence),
            );
        match row_checkpoint(state, source, binding)? {
            Some(position) => {
                anyhow::ensure!(
                    position.managed_slot.is_none(),
                    "shared PostgreSQL progress changed its configured-slot ownership"
                );
                max_counter = max_counter.max(position.sequence);
                max_lsn = max_lsn.max(position.commit_lsn);
                boundaries.insert(position.feedback_lsn());
            }
            None => missing = true,
        }
    }
    Ok(SharedObservation {
        minimum,
        max_counter,
        max_lsn,
        boundaries,
        missing,
        ready,
    })
}

pub(crate) struct SharedUpdates {
    pub updates: Box<dyn ConsumerProgressUpdates>,
    pub expected: BTreeSet<ComponentId>,
    history: Arc<Mutex<Option<SharedHistory>>>,
    fresh_lsn: Option<u64>,
    unready: bool,
    mode: SharedMode,
}

struct SharedHistory {
    binding: [u8; 32],
    generations: BTreeMap<ComponentId, u64>,
    initialized: BTreeSet<ComponentId>,
    fresh_lsn: Option<u64>,
    mode: SharedMode,
}

impl SharedHistory {
    fn restart(&mut self, snapshot: &ConsumerProgressSnapshot, source: &ComponentId) -> Result<()> {
        for (id, state) in &snapshot.consumers {
            if self.generations.get(id) != Some(&state.reset_generation) {
                let position = self.mode.cursor(state, source, Some(self.binding))?;
                anyhow::ensure!(
                    self.generations.get(id).is_some_and(|previous| state.reset_generation > *previous)
                        && state.ready && state.recovered && state.admitting
                        && state.failure.is_none() && state.bootstrap_complete
                        && position == Some(true),
                    "PostgreSQL consumer {id} reset requires a completed configured-slot snapshot before source restart"
                );
            }
        }
        for (id, state) in &snapshot.consumers {
            self.generations.insert(id.clone(), state.reset_generation);
        }
        self.observe(snapshot, source)
    }

    fn observe(&mut self, snapshot: &ConsumerProgressSnapshot, source: &ComponentId) -> Result<()> {
        anyhow::ensure!(
            snapshot.consumers.keys().eq(self.generations.keys()),
            "PostgreSQL consumer membership changed"
        );
        for (id, state) in &snapshot.consumers {
            anyhow::ensure!(
                self.generations.get(id) == Some(&state.reset_generation),
                "PostgreSQL consumer {id} was reset; source reconstruction is required"
            );
            if self
                .mode
                .cursor(state, source, Some(self.binding))?
                .is_some()
            {
                self.initialized.insert(id.clone());
            } else {
                anyhow::ensure!(
                    !self.initialized.contains(id) && self.fresh_lsn.is_some(),
                    "PostgreSQL consumer {id} lost its handled input cursor"
                );
                anyhow::ensure!(
                    !state.bootstrap_complete,
                    "PostgreSQL consumer {id} needs explicit bootstrap enrollment"
                );
            }
        }
        Ok(())
    }
}

impl SharedUpdates {
    pub fn new(
        binding: &SharedBinding,
        initial: &ConsumerProgressSnapshot,
        source: &ComponentId,
        database_binding: [u8; 32],
        fresh_lsn: Option<u64>,
    ) -> Result<Self> {
        {
            let mut history = binding
                .history
                .lock()
                .map_err(|_| anyhow::anyhow!("PostgreSQL consumer history poisoned"))?;
            let history = history.get_or_insert_with(|| SharedHistory {
                binding: database_binding,
                generations: initial
                    .consumers
                    .iter()
                    .map(|(id, state)| (id.clone(), state.reset_generation))
                    .collect(),
                initialized: BTreeSet::new(),
                fresh_lsn,
                mode: binding.mode,
            });
            anyhow::ensure!(
                history.binding == database_binding && history.mode == binding.mode,
                "PostgreSQL shared binding changed across source restart"
            );
            history.restart(initial, source)?;
        }
        Ok(Self {
            updates: binding.reader.subscribe()?,
            expected: binding.expected.clone(),
            history: binding.history.clone(),
            fresh_lsn,
            unready: false,
            mode: binding.mode,
        })
    }

    pub fn snapshot(
        &mut self,
        source: &ComponentId,
        stream: &StreamId,
        binding: [u8; 32],
        persistent: bool,
    ) -> Result<SharedObservation> {
        anyhow::ensure!(
            self.mode == SharedMode::Rows,
            "PostgreSQL progress mode is not rows"
        );
        let snapshot = self.updates.snapshot()?;
        let observed = observe(
            &snapshot,
            &self.expected,
            source,
            stream,
            binding,
            persistent,
        )?;
        self.observe_history(&snapshot, source, observed.ready)?;
        Ok(observed)
    }

    pub fn transaction_snapshot(
        &mut self,
        source: &ComponentId,
        stream: &StreamId,
        binding: [u8; 32],
    ) -> Result<SharedTransactionObservation> {
        anyhow::ensure!(
            self.mode == SharedMode::Transactions,
            "PostgreSQL progress mode is not transactions"
        );
        let snapshot = self.updates.snapshot()?;
        let observed = observe_transactions(&snapshot, &self.expected, source, stream, binding)?;
        self.observe_history(&snapshot, source, observed.ready)?;
        Ok(observed)
    }

    fn observe_history(
        &mut self,
        snapshot: &ConsumerProgressSnapshot,
        source: &ComponentId,
        ready: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.unready || !ready,
            "PostgreSQL consumer readiness changed; awaited source rewind and resubscription are required"
        );
        self.unready |= !ready;
        self.history
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL consumer history poisoned"))?
            .as_mut()
            .context("PostgreSQL shared history is missing")?
            .observe(snapshot, source)
    }

    pub fn initial_lsn(&self) -> Result<u64> {
        self.fresh_lsn
            .context("PostgreSQL consumer initialization has no retained streaming origin")
    }
}

pub(crate) struct SharedTransactionObservation {
    pub minimum: Option<TransactionProgressMinimum>,
    pub max_counter: u64,
    pub max_lsn: u64,
    pub missing: bool,
    pub ready: bool,
}

pub(crate) fn observe_transactions(
    snapshot: &ConsumerProgressSnapshot,
    expected: &BTreeSet<ComponentId>,
    source: &ComponentId,
    stream: &StreamId,
    binding: [u8; 32],
) -> Result<SharedTransactionObservation> {
    let ready = validate_members(snapshot, expected, true)?;
    let minimum = transactions::minimum_checkpoint(snapshot, expected, source, binding)?;
    let mut max_counter = 0;
    let mut max_lsn = 0;
    let mut missing = false;
    for state in snapshot.consumers.values() {
        max_counter = max_counter
            .max(state.transport_sequences.get(stream).copied().unwrap_or(0))
            .max(
                state
                    .checkpoints
                    .get(&SourceProgressKey::Stream(stream.clone()))
                    .map_or(0, |checkpoint| checkpoint.sequence),
            );
        if let Some(position) = transactions::checkpoint(state, source, binding)? {
            max_counter = max_counter.max(position.sequence);
            max_lsn = max_lsn.max(position.commit_lsn);
        } else {
            missing = true;
        }
    }
    Ok(SharedTransactionObservation {
        minimum,
        max_counter,
        max_lsn,
        missing,
        ready,
    })
}

pub(crate) struct TransactionEmission {
    counters: Arc<SharedCounters>,
    transport: Arc<AtomicU64>,
}

impl TransactionEmission {
    pub fn new(
        counters: Arc<SharedCounters>,
        transport: Arc<AtomicU64>,
        counter: u64,
        lsn: u64,
    ) -> Self {
        counters.raise(&transport, counter, lsn);
        Self {
            counters,
            transport,
        }
    }

    pub fn position(
        &self,
        lsn: u64,
        binding: [u8; 32],
        managed_slot: Option<&str>,
    ) -> Result<(Vec<u8>, u64)> {
        anyhow::ensure!(
            managed_slot.is_none(),
            "shared transactions cannot claim a managed slot"
        );
        let logical = self.counters.allocate(&self.transport)?;
        let position = SharedTransactionPosition::commit(binding, lsn, logical)?;
        Ok((serde_json::to_vec(&position)?, logical))
    }

    pub fn admit(&self, lsn: u64) {
        self.counters
            .admitted_lsn
            .fetch_max(lsn, AtomicOrdering::Relaxed);
    }

    pub fn admitted_lsn(&self) -> u64 {
        self.counters.admitted_lsn.load(AtomicOrdering::Relaxed)
    }

    pub fn validate_observation(&self, counter: u64, lsn: u64) -> Result<()> {
        anyhow::ensure!(
            counter
                <= self
                    .counters
                    .logical
                    .load(AtomicOrdering::Relaxed)
                    .max(self.transport.load(AtomicOrdering::Relaxed))
                && lsn <= self.admitted_lsn(),
            "PostgreSQL shared cursor moved beyond admitted whole transactions"
        );
        Ok(())
    }
}
