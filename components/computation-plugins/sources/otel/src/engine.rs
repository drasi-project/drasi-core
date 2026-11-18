// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Admission of projected OTLP data and TTL expiry, shared by both transports.
//!
//! One mutex serializes every projection, admission and expiry, so the
//! insert-or-update decision for an element always matches the order in
//! which the graph receives its changes. Each group is admitted
//! all-or-nothing ([`Ingress::admit_all`]); when admission refuses a group,
//! the state that group recorded is undone, and the request fails with the
//! refusal so the exporter retries it. Groups admitted before the refusal
//! stay admitted: a retry updates them again rather than duplicating them,
//! because element identities are derived from the data.

use crate::config::Projection;
use crate::projection::MapOutcome;
use crate::state::{delete_change, groups, GroupOutcome, GroupUndo, ProjectionState};
use drasi_computation_network_common::diag;
use drasi_computation_network_common::ingress::{Ingress, IngressError, Live};
use drasi_core_types::models::SourceChange;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

/// Process-lifetime admission counters of one source object.
#[derive(Debug, Default)]
pub struct Counters {
    /// Records that produced admitted graph changes.
    pub accepted: AtomicU64,
    /// Records refused by an allowlist, filter, schema rule or timestamp check.
    pub rejected: AtomicU64,
    /// Records dropped because a cardinality cap was reached.
    pub dropped: AtomicU64,
    /// Elements deleted after their TTL elapsed.
    pub expired: AtomicU64,
}

/// A point-in-time copy of [`Counters`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterSnapshot {
    pub accepted: u64,
    pub rejected: u64,
    pub dropped: u64,
    pub expired: u64,
}

impl Counters {
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            expired: self.expired.load(Ordering::Relaxed),
        }
    }
}

/// The outcome of one successfully admitted export request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportOutcome {
    /// Records not admitted: rejected by projection or dropped at a cap.
    /// Reported to the exporter as OTLP partial success.
    pub rejected: u64,
}

/// Projection state, settings and counters of one source object.
pub struct Engine {
    namespace: String,
    settings: Live<Projection>,
    state: Mutex<ProjectionState>,
    counters: Counters,
}

/// The current wall-clock time in Unix milliseconds.
pub fn now_millis() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

impl Engine {
    /// `namespace` is the element namespace: the source component's UUID.
    pub fn new(namespace: impl Into<String>, settings: Projection) -> Self {
        Self {
            namespace: namespace.into(),
            settings: Live::new(settings),
            state: Mutex::new(ProjectionState::default()),
            counters: Counters::default(),
        }
    }

    /// Replaces the projection settings; the next request uses them.
    pub fn update_settings(&self, settings: Projection) {
        self.settings.store(settings);
    }

    pub fn counters(&self) -> CounterSnapshot {
        self.counters.snapshot()
    }

    /// Exclusive access to the state, for inspection.
    pub async fn state(&self) -> tokio::sync::MutexGuard<'_, ProjectionState> {
        self.state.lock().await
    }

    /// Projects and admits one export request received now.
    pub async fn ingest(
        &self,
        ingress: &Ingress,
        project: impl FnOnce(&Projection, u64) -> MapOutcome,
    ) -> Result<ExportOutcome, IngressError> {
        self.ingest_at(ingress, now_millis(), project).await
    }

    /// [`Self::ingest`] with an explicit receipt time.
    pub async fn ingest_at(
        &self,
        ingress: &Ingress,
        received_at_millis: u64,
        project: impl FnOnce(&Projection, u64) -> MapOutcome,
    ) -> Result<ExportOutcome, IngressError> {
        // A paused or stopping source refuses the request before projecting,
        // even when none of its records would have produced a change.
        if ingress.shutdown.is_cancelled() {
            return Err(IngressError::stopped());
        }
        if ingress.is_paused() {
            return Err(IngressError::paused());
        }
        let settings = self.settings.load();
        let outcome = project(&settings, received_at_millis);
        self.counters
            .rejected
            .fetch_add(outcome.rejected, Ordering::Relaxed);
        let mut dropped = 0u64;
        let mut state = self.state.lock().await;
        for group in groups(outcome.elements) {
            match state.apply_group(&self.namespace, &settings, group, received_at_millis) {
                GroupOutcome::Dropped => dropped += 1,
                GroupOutcome::Admitted(changes, undo) => {
                    // `admit_all` queues all or none of the changes, also when
                    // the request is cancelled (an exporter deadline or a
                    // dropped connection), so the guard undoes the state
                    // unless the changes were queued.
                    let pending = PendingUndo {
                        state: &mut state,
                        undo: Some(undo),
                    };
                    if let Err(error) = ingress.admit_all(changes).await {
                        drop(pending);
                        self.counters.dropped.fetch_add(dropped, Ordering::Relaxed);
                        return Err(error);
                    }
                    pending.commit();
                }
            }
        }
        drop(state);
        if dropped > 0 {
            diag!(
                Debug,
                "OTLP groups dropped at a cardinality cap: component={} dropped={dropped}",
                self.namespace
            );
        }
        self.counters.dropped.fetch_add(dropped, Ordering::Relaxed);
        self.counters
            .accepted
            .fetch_add(outcome.accepted.saturating_sub(dropped), Ordering::Relaxed);
        Ok(ExportOutcome {
            rejected: outcome.rejected.saturating_add(dropped),
        })
    }

    /// Reconciles the state with changes Stop discarded undelivered; see
    /// [`ProjectionState::reconcile_discarded`].
    pub async fn reconcile_discarded(&self, discarded: &[SourceChange]) -> usize {
        self.state.lock().await.reconcile_discarded(discarded)
    }

    /// Deletes every element whose TTL has elapsed now.
    pub async fn sweep(&self, ingress: &Ingress) -> Result<usize, IngressError> {
        self.sweep_at(ingress, now_millis()).await
    }

    /// [`Self::sweep`] at an explicit time. Deletes are admitted in chunks
    /// that fit the ingress queue; each admitted chunk is forgotten, and a
    /// refused chunk stays scheduled for the next sweep. A paused source
    /// defers expiry until Resume.
    pub async fn sweep_at(
        &self,
        ingress: &Ingress,
        now_millis: u64,
    ) -> Result<usize, IngressError> {
        if ingress.is_paused() || ingress.shutdown.is_cancelled() {
            return Ok(0);
        }
        let mut state = self.state.lock().await;
        let due = state.due(now_millis);
        let mut expired = 0;
        for records in due.chunks(ingress.config.ingress_capacity.max(1)) {
            let deletes = records
                .iter()
                .map(|record| delete_change(&self.namespace, record, now_millis))
                .collect();
            ingress.admit_all(deletes).await?;
            state.expire(records);
            expired += records.len();
            self.counters
                .expired
                .fetch_add(records.len() as u64, Ordering::Relaxed);
        }
        Ok(expired)
    }
}

/// Undoes one group's state changes when dropped before [`Self::commit`].
struct PendingUndo<'a> {
    state: &'a mut ProjectionState,
    undo: Option<GroupUndo>,
}

impl PendingUndo<'_> {
    fn commit(mut self) {
        self.undo = None;
    }
}

impl Drop for PendingUndo<'_> {
    fn drop(&mut self) {
        if let Some(undo) = self.undo.take() {
            self.state.undo(undo);
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
