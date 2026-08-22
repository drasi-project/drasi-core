// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::config::GitHubWorkGraphDispatcherConfig;
use crate::github::{GitHubApi, PostDisposition, RemoteComment};
use crate::model::{
    sha256_digest, BootstrapWatermark, CapacityRow, DispatcherIdentity, Reservation,
    ReservationPhase, WorkerCursor, BOOTSTRAP_WATERMARK_KEY, IDENTITY_KEY, RESERVATION_PREFIX,
};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, TimeDelta, Timelike, Utc};
use drasi_github_workgraph::{candidate_task_lease_id, canonical_task_lease_body, TaskLease};
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::state_store::StateStoreProvider;
use log::info;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

#[async_trait]
pub(crate) trait Clock: Send + Sync {
    async fn now(&self) -> DateTime<Utc>;
}

pub(crate) struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    async fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub(crate) trait LeaseIdGenerator: Send + Sync {
    fn generate(&self) -> String;
}

pub(crate) struct UuidV7Generator;

impl LeaseIdGenerator for UuidV7Generator {
    fn generate(&self) -> String {
        uuid::Uuid::now_v7().to_string()
    }
}

#[derive(Debug)]
struct RowChange {
    row: CapacityRow,
    row_signature: u64,
    deleted: bool,
}

enum RemoteLeaseState {
    Absent,
    Exact(RemoteComment),
    Conflict(String),
}

pub(crate) struct DispatcherEngine {
    reaction_id: String,
    query_id: String,
    config: GitHubWorkGraphDispatcherConfig,
    state_store: Arc<dyn StateStoreProvider>,
    github: Arc<dyn GitHubApi>,
    clock: Arc<dyn Clock>,
    lease_ids: Arc<dyn LeaseIdGenerator>,
    reservations: BTreeMap<String, Reservation>,
    bootstrap_watermark: Option<u64>,
}

impl DispatcherEngine {
    pub(crate) fn new(
        reaction_id: String,
        query_id: String,
        config: GitHubWorkGraphDispatcherConfig,
        state_store: Arc<dyn StateStoreProvider>,
        github: Arc<dyn GitHubApi>,
        clock: Arc<dyn Clock>,
        lease_ids: Arc<dyn LeaseIdGenerator>,
    ) -> Self {
        Self {
            reaction_id,
            query_id,
            config,
            state_store,
            github,
            clock,
            lease_ids,
            reservations: BTreeMap::new(),
            bootstrap_watermark: None,
        }
    }

    pub(crate) async fn recover(&mut self) -> Result<()> {
        self.bind_identity().await?;
        self.load_bootstrap_watermark().await?;
        self.load_reservations().await?;
        let lease_ids: Vec<String> = self.reservations.keys().cloned().collect();
        for lease_id in lease_ids {
            let phase = self
                .reservations
                .get(&lease_id)
                .context("loaded reservation disappeared")?
                .phase;
            match phase {
                ReservationPhase::Reserved => self.write_reservation(&lease_id).await?,
                ReservationPhase::WriteInFlight => self.recover_write_in_flight(&lease_id).await?,
                ReservationPhase::AwaitingProjection => {
                    self.verify_awaiting_projection(&lease_id).await?
                }
                ReservationPhase::Confirmed => self.delete_reservation(&lease_id).await?,
                ReservationPhase::ReconcileRequired => {
                    bail!("reservation '{lease_id}' requires operator reconciliation")
                }
            }
        }
        Ok(())
    }

    async fn bind_identity(&self) -> Result<()> {
        let identity = DispatcherIdentity {
            schema_version: 1,
            query_id: self.query_id.clone(),
            api_url: self.config.normalized_api_url(),
        };
        if let Some(bytes) = self
            .state_store
            .get(&self.reaction_id, IDENTITY_KEY)
            .await
            .context("failed to read dispatcher identity")?
        {
            let persisted: DispatcherIdentity =
                serde_json::from_slice(&bytes).context("dispatcher identity is corrupt")?;
            ensure!(
                persisted == identity,
                "durable dispatcher state is bound to query '{}' at API target '{}', not query '{}' \
                 at '{}'",
                persisted.query_id,
                persisted.api_url,
                identity.query_id,
                identity.api_url
            );
            return Ok(());
        }
        self.state_store
            .set(
                &self.reaction_id,
                IDENTITY_KEY,
                serde_json::to_vec(&identity).context("failed to serialize dispatcher identity")?,
            )
            .await
            .context("failed to persist dispatcher identity")?;
        self.state_store
            .sync()
            .await
            .context("failed to sync dispatcher identity")
    }

    async fn load_bootstrap_watermark(&mut self) -> Result<()> {
        let Some(bytes) = self
            .state_store
            .get(&self.reaction_id, BOOTSTRAP_WATERMARK_KEY)
            .await
            .context("failed to read dispatcher bootstrap watermark")?
        else {
            return Ok(());
        };
        let watermark: BootstrapWatermark =
            serde_json::from_slice(&bytes).context("dispatcher bootstrap watermark is corrupt")?;
        ensure!(
            watermark.schema_version == 1 && watermark.query_id == self.query_id,
            "dispatcher bootstrap watermark identity is corrupt"
        );
        self.bootstrap_watermark = Some(watermark.sequence);
        Ok(())
    }

    pub(crate) async fn persist_bootstrap_watermark(&mut self, sequence: u64) -> Result<()> {
        if self
            .bootstrap_watermark
            .is_some_and(|watermark| sequence <= watermark)
        {
            return Ok(());
        }
        let watermark = BootstrapWatermark {
            schema_version: 1,
            query_id: self.query_id.clone(),
            sequence,
        };
        self.state_store
            .set(
                &self.reaction_id,
                BOOTSTRAP_WATERMARK_KEY,
                serde_json::to_vec(&watermark)
                    .context("failed to serialize dispatcher bootstrap watermark")?,
            )
            .await
            .context("failed to persist dispatcher bootstrap watermark")?;
        self.state_store
            .sync()
            .await
            .context("failed to sync dispatcher bootstrap watermark")?;
        self.bootstrap_watermark = Some(sequence);
        Ok(())
    }

    async fn load_reservations(&mut self) -> Result<()> {
        let mut keys = self
            .state_store
            .list_keys(&self.reaction_id)
            .await
            .context("failed to list dispatcher state")?;
        keys.sort();
        let mut tasks = HashSet::new();
        let mut slots = HashSet::new();
        for key in keys {
            if !key.starts_with(RESERVATION_PREFIX) {
                continue;
            }
            let bytes = self
                .state_store
                .get(&self.reaction_id, &key)
                .await
                .context("failed to read dispatcher reservation")?
                .with_context(|| format!("reservation key '{key}' disappeared while loading"))?;
            let reservation: Reservation = serde_json::from_slice(&bytes)
                .with_context(|| format!("reservation '{key}' is not valid JSON"))?;
            reservation
                .validate()
                .with_context(|| format!("reservation '{key}' is corrupt"))?;
            ensure!(
                reservation.key() == key,
                "reservation key does not match its leaseId"
            );
            if reservation.phase.overlays_capacity() {
                ensure!(
                    tasks.insert(reservation.task_node_id.clone()),
                    "multiple pending reservations claim task '{}'",
                    reservation.task_node_id
                );
                ensure!(
                    slots.insert(reservation.slot_id.clone()),
                    "multiple pending reservations claim slot '{}'",
                    reservation.slot_id
                );
            }
            ensure!(
                self.reservations
                    .insert(reservation.lease_id.clone(), reservation)
                    .is_none(),
                "duplicate durable reservation leaseId"
            );
        }
        Ok(())
    }

    pub(crate) async fn process(&mut self, result: &QueryResult) -> Result<()> {
        ensure!(
            result.query_id == self.query_id,
            "received result from unexpected query '{}'",
            result.query_id
        );
        if self
            .bootstrap_watermark
            .is_some_and(|watermark| result.sequence <= watermark)
        {
            return Ok(());
        }
        let mut changes = Vec::new();
        let mut workers = HashSet::new();
        for diff in &result.results {
            if let Some(change) = decode_change(diff)? {
                change.row.validate()?;
                ensure!(
                    workers.insert(change.row.worker_id.clone()),
                    "one QueryResult may contain at most one change per worker"
                );
                changes.push(change);
            }
        }

        for change in changes {
            match self
                .cursor_disposition(
                    &result.query_id,
                    &change.row.worker_id,
                    result.sequence,
                    change.row_signature,
                )
                .await?
            {
                CursorDisposition::Replay => continue,
                CursorDisposition::Current => {}
            }

            self.reconcile_capacity_row(&change.row, !change.deleted)
                .await?;
            if !change.deleted {
                self.dispatch_capacity(&change.row, result.sequence, change.row_signature)
                    .await?;
            }
            self.persist_cursor(WorkerCursor {
                schema_version: 1,
                query_id: result.query_id.clone(),
                worker_id: change.row.worker_id,
                sequence: result.sequence,
                row_signature: change.row_signature,
            })
            .await?;
        }
        Ok(())
    }

    async fn cursor_disposition(
        &self,
        query_id: &str,
        worker_id: &str,
        sequence: u64,
        row_signature: u64,
    ) -> Result<CursorDisposition> {
        let key = WorkerCursor::key(query_id, worker_id);
        let Some(bytes) = self
            .state_store
            .get(&self.reaction_id, &key)
            .await
            .context("failed to read dispatcher cursor")?
        else {
            return Ok(CursorDisposition::Current);
        };
        let cursor: WorkerCursor =
            serde_json::from_slice(&bytes).context("dispatcher cursor is corrupt")?;
        ensure!(
            cursor.schema_version == 1
                && cursor.query_id == query_id
                && cursor.worker_id == worker_id,
            "dispatcher cursor identity is corrupt"
        );
        if sequence < cursor.sequence {
            return Ok(CursorDisposition::Replay);
        }
        if sequence == cursor.sequence {
            ensure!(
                row_signature == cursor.row_signature,
                "query sequence {sequence} replayed with a different row signature for worker \
                 '{worker_id}'"
            );
            return Ok(CursorDisposition::Replay);
        }
        Ok(CursorDisposition::Current)
    }

    async fn persist_cursor(&self, cursor: WorkerCursor) -> Result<()> {
        let key = WorkerCursor::key(&cursor.query_id, &cursor.worker_id);
        let bytes = serde_json::to_vec(&cursor).context("failed to serialize dispatcher cursor")?;
        self.state_store
            .set(&self.reaction_id, &key, bytes)
            .await
            .context("failed to persist dispatcher cursor")?;
        self.state_store
            .sync()
            .await
            .context("failed to sync dispatcher cursor")
    }

    async fn reconcile_capacity_row(&mut self, row: &CapacityRow, current: bool) -> Result<()> {
        let matching_ids: Vec<String> = self
            .reservations
            .values()
            .filter(|reservation| {
                reservation.worker_id == row.worker_id
                    && reservation.repository_owner == row.repository_owner
                    && reservation.repository_name == row.repository_name
                    && reservation.phase.overlays_capacity()
            })
            .map(|reservation| reservation.lease_id.clone())
            .collect();

        for lease_id in matching_ids {
            if row.active_lease_ids.contains(&lease_id) {
                let reservation = self
                    .reservations
                    .get(&lease_id)
                    .context("reservation disappeared during row confirmation")?;
                let slot_still_free = row.free_slot_ids.contains(&reservation.slot_id);
                let task_still_dispatchable = row
                    .dispatchable_task_ids
                    .contains(&reservation.task_node_id);
                if slot_still_free || task_still_dispatchable {
                    let reason = format!(
                        "capacity row confirms leaseId '{}' while its reserved {}{} still appears \
                         available",
                        lease_id,
                        if slot_still_free { "slot" } else { "" },
                        if task_still_dispatchable {
                            if slot_still_free {
                                " and task"
                            } else {
                                "task"
                            }
                        } else {
                            ""
                        }
                    );
                    self.require_reconciliation(&lease_id, reason).await?;
                }
                self.confirm_reservation(&lease_id).await?;
                continue;
            }
            if !current {
                continue;
            }
            let reservation = self
                .reservations
                .get(&lease_id)
                .context("reservation disappeared during row reconciliation")?;
            let slot_still_free = row.free_slot_ids.contains(&reservation.slot_id);
            let task_still_dispatchable = row
                .dispatchable_task_ids
                .contains(&reservation.task_node_id);
            if !slot_still_free || !task_still_dispatchable {
                let reason = format!(
                    "capacity row dropped reserved {}{} without confirming leaseId '{}'",
                    if !slot_still_free { "slot" } else { "" },
                    if !task_still_dispatchable {
                        if !slot_still_free {
                            " and task"
                        } else {
                            "task"
                        }
                    } else {
                        ""
                    },
                    lease_id
                );
                self.require_reconciliation(&lease_id, reason).await?;
            }
        }
        Ok(())
    }

    async fn dispatch_capacity(
        &mut self,
        row: &CapacityRow,
        sequence: u64,
        row_signature: u64,
    ) -> Result<()> {
        let claimed_slots: HashSet<String> = self
            .reservations
            .values()
            .filter(|reservation| reservation.phase.overlays_capacity())
            .map(|reservation| reservation.slot_id.clone())
            .collect();
        let claimed_tasks: HashSet<String> = self
            .reservations
            .values()
            .filter(|reservation| reservation.phase.overlays_capacity())
            .map(|reservation| reservation.task_node_id.clone())
            .collect();
        let slots: Vec<&String> = row
            .free_slot_ids
            .iter()
            .filter(|slot_id| !claimed_slots.contains(*slot_id))
            .collect();
        let tasks: Vec<_> = row
            .dispatchable_tasks
            .iter()
            .filter(|task| !claimed_tasks.contains(&task.task_node_id))
            .collect();

        for (slot_id, task) in slots.into_iter().zip(tasks) {
            let now = self
                .clock
                .now()
                .await
                .with_nanosecond(0)
                .context("clock produced an invalid timestamp")?;
            let expires = now
                .checked_add_signed(TimeDelta::seconds(row.lease_duration_seconds))
                .context("lease expiration timestamp overflowed")?;
            let lease_id = self.lease_ids.generate();
            ensure!(
                !self.reservations.contains_key(&lease_id),
                "lease ID generator returned duplicate identifier '{lease_id}'"
            );
            let lease = TaskLease {
                lease_id: lease_id.clone(),
                assignment_comment_node_id: task.assignment_comment_node_id.clone(),
                worker_id: row.worker_id.clone(),
                slot_id: (*slot_id).clone(),
                acquired_at: canonical_time(now),
                expires_at: canonical_time(expires),
            };
            let body = canonical_task_lease_body(&lease).map_err(anyhow::Error::msg)?;
            let reservation = Reservation {
                schema_version: 1,
                lease_id: lease_id.clone(),
                query_id: self.query_id.clone(),
                worker_id: row.worker_id.clone(),
                agent_profile: row.agent_profile.clone(),
                repository_owner: task.repository_owner.clone(),
                repository_name: task.repository_name.clone(),
                task_node_id: task.task_node_id.clone(),
                task_number: task.task_number,
                assignment_comment_node_id: task.assignment_comment_node_id.clone(),
                slot_id: (*slot_id).clone(),
                task_type: task.task_type.clone(),
                acquired_at: lease.acquired_at,
                expires_at: lease.expires_at,
                canonical_body: body.clone(),
                body_digest: sha256_digest(&body),
                phase: ReservationPhase::Reserved,
                attempt_count: 0,
                last_error: None,
                origin_sequence: sequence,
                origin_row_signature: row_signature,
                lease_comment_node_id: None,
                lease_comment_database_id: None,
            };
            reservation.validate()?;
            self.persist_reservation(&reservation).await?;
            self.reservations.insert(lease_id.clone(), reservation);
            self.write_reservation(&lease_id).await?;
        }
        Ok(())
    }

    async fn write_reservation(&mut self, lease_id: &str) -> Result<()> {
        loop {
            let attempts = self
                .reservations
                .get(lease_id)
                .context("reservation disappeared before write")?
                .attempt_count;
            if attempts >= self.config.max_attempts {
                return self
                    .require_reconciliation(
                        lease_id,
                        format!(
                            "GitHub Lease write remained absent after {} attempts",
                            self.config.max_attempts
                        ),
                    )
                    .await;
            }

            {
                let reservation = self
                    .reservations
                    .get_mut(lease_id)
                    .context("reservation disappeared before write")?;
                reservation.phase = ReservationPhase::WriteInFlight;
                reservation.attempt_count += 1;
                reservation.last_error = None;
            }
            self.persist_current(lease_id).await?;
            let reservation = self
                .reservations
                .get(lease_id)
                .context("reservation disappeared before HTTP write")?
                .clone();
            let disposition = self
                .github
                .post_comment(
                    &reservation.repository_owner,
                    &reservation.repository_name,
                    reservation.task_number,
                    &reservation.canonical_body,
                )
                .await;
            match disposition {
                PostDisposition::Accepted(comment) => {
                    self.await_projection(lease_id, comment).await?;
                    return Ok(());
                }
                PostDisposition::Rejected(reason) => {
                    return self.require_reconciliation(lease_id, reason).await;
                }
                PostDisposition::Ambiguous {
                    reason,
                    retry_after,
                } => {
                    self.record_error(lease_id, &reason).await?;
                    match self.reconcile_remote(lease_id).await? {
                        RemoteLeaseState::Exact(comment) => {
                            self.await_projection(lease_id, comment).await?;
                            return Ok(());
                        }
                        RemoteLeaseState::Conflict(reason) => {
                            return self.require_reconciliation(lease_id, reason).await;
                        }
                        RemoteLeaseState::Absent => {
                            let attempts = self
                                .reservations
                                .get(lease_id)
                                .context("reservation disappeared after reconciliation")?
                                .attempt_count;
                            let delay = retry_after.map_or_else(
                                || self.retry_delay(attempts),
                                |retry_after| self.retry_delay(attempts).max(retry_after),
                            );
                            tokio::time::sleep(delay).await;
                            match self.reconcile_remote(lease_id).await? {
                                RemoteLeaseState::Exact(comment) => {
                                    self.await_projection(lease_id, comment).await?;
                                    return Ok(());
                                }
                                RemoteLeaseState::Conflict(reason) => {
                                    return self.require_reconciliation(lease_id, reason).await;
                                }
                                RemoteLeaseState::Absent => {
                                    if attempts >= self.config.max_attempts {
                                        return self
                                            .require_reconciliation(
                                                lease_id,
                                                format!(
                                                    "GitHub Lease write was proven absent after \
                                                     {attempts} attempts"
                                                ),
                                            )
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn recover_write_in_flight(&mut self, lease_id: &str) -> Result<()> {
        match self.reconcile_remote(lease_id).await? {
            RemoteLeaseState::Exact(comment) => self.await_projection(lease_id, comment).await,
            RemoteLeaseState::Conflict(reason) => {
                self.require_reconciliation(lease_id, reason).await
            }
            RemoteLeaseState::Absent => {
                let attempts = self
                    .reservations
                    .get(lease_id)
                    .context("reservation disappeared during write recovery")?
                    .attempt_count
                    .max(1);
                tokio::time::sleep(self.retry_delay(attempts)).await;
                match self.reconcile_remote(lease_id).await? {
                    RemoteLeaseState::Exact(comment) => {
                        self.await_projection(lease_id, comment).await
                    }
                    RemoteLeaseState::Conflict(reason) => {
                        self.require_reconciliation(lease_id, reason).await
                    }
                    RemoteLeaseState::Absent => self.write_reservation(lease_id).await,
                }
            }
        }
    }

    async fn verify_awaiting_projection(&mut self, lease_id: &str) -> Result<()> {
        match self.reconcile_remote(lease_id).await? {
            RemoteLeaseState::Exact(comment) => self.await_projection(lease_id, comment).await,
            RemoteLeaseState::Conflict(reason) => {
                self.require_reconciliation(lease_id, reason).await
            }
            RemoteLeaseState::Absent => {
                self.require_reconciliation(
                    lease_id,
                    "an awaiting-projection Lease comment is no longer present on GitHub"
                        .to_string(),
                )
                .await
            }
        }
    }

    async fn reconcile_remote(&self, lease_id: &str) -> Result<RemoteLeaseState> {
        let reservation = self
            .reservations
            .get(lease_id)
            .context("reservation disappeared before reconciliation")?;
        let comments = self
            .github
            .list_comments(
                &reservation.repository_owner,
                &reservation.repository_name,
                reservation.task_number,
            )
            .await?;
        let mut exact = Vec::new();
        let mut conflicting = 0usize;
        for comment in comments {
            if comment.body == reservation.canonical_body {
                exact.push(comment);
                continue;
            }
            if candidate_task_lease_id(&comment.body).as_deref() == Some(lease_id) {
                conflicting += 1;
            }
        }
        if exact.len() == 1 && conflicting == 0 {
            return Ok(RemoteLeaseState::Exact(exact.remove(0)));
        }
        if exact.is_empty() && conflicting == 0 {
            return Ok(RemoteLeaseState::Absent);
        }
        Ok(RemoteLeaseState::Conflict(format!(
            "GitHub contains {} exact and {conflicting} conflicting comments for leaseId \
             '{lease_id}'",
            exact.len()
        )))
    }

    async fn await_projection(&mut self, lease_id: &str, comment: RemoteComment) -> Result<()> {
        let reservation = self
            .reservations
            .get_mut(lease_id)
            .context("reservation disappeared before awaiting projection")?;
        ensure!(
            comment.body == reservation.canonical_body,
            "GitHub reconciliation returned a mismatched comment body"
        );
        reservation.phase = ReservationPhase::AwaitingProjection;
        reservation.last_error = None;
        reservation.lease_comment_node_id = Some(comment.node_id);
        reservation.lease_comment_database_id = Some(comment.database_id);
        self.persist_current(lease_id).await
    }

    async fn confirm_reservation(&mut self, lease_id: &str) -> Result<()> {
        let reservation = self
            .reservations
            .get_mut(lease_id)
            .context("reservation disappeared before confirmation")?;
        reservation.phase = ReservationPhase::Confirmed;
        reservation.last_error = None;
        self.persist_current(lease_id).await?;
        self.delete_reservation(lease_id).await
    }

    async fn require_reconciliation(&mut self, lease_id: &str, reason: String) -> Result<()> {
        let reason = bounded_error(&reason);
        let reservation = self
            .reservations
            .get_mut(lease_id)
            .context("reservation disappeared before reconciliation failure")?;
        reservation.phase = ReservationPhase::ReconcileRequired;
        reservation.last_error = Some(reason.clone());
        self.persist_current(lease_id).await?;
        bail!("reservation '{lease_id}' requires reconciliation: {reason}")
    }

    async fn record_error(&mut self, lease_id: &str, reason: &str) -> Result<()> {
        self.reservations
            .get_mut(lease_id)
            .context("reservation disappeared before error persistence")?
            .last_error = Some(bounded_error(reason));
        self.persist_current(lease_id).await
    }

    async fn persist_current(&self, lease_id: &str) -> Result<()> {
        let reservation = self
            .reservations
            .get(lease_id)
            .context("reservation disappeared before persistence")?;
        self.persist_reservation(reservation).await
    }

    async fn persist_reservation(&self, reservation: &Reservation) -> Result<()> {
        reservation.validate()?;
        let bytes =
            serde_json::to_vec(reservation).context("failed to serialize reservation ledger")?;
        self.state_store
            .set(&self.reaction_id, &reservation.key(), bytes)
            .await
            .context("failed to persist reservation ledger")?;
        self.state_store
            .sync()
            .await
            .context("failed to sync reservation ledger")
    }

    async fn delete_reservation(&mut self, lease_id: &str) -> Result<()> {
        let key = self
            .reservations
            .get(lease_id)
            .context("reservation disappeared before deletion")?
            .key();
        self.state_store
            .delete(&self.reaction_id, &key)
            .await
            .context("failed to delete confirmed reservation")?;
        self.state_store
            .sync()
            .await
            .context("failed to sync confirmed reservation deletion")?;
        self.reservations.remove(lease_id);
        Ok(())
    }

    fn retry_delay(&self, completed_attempts: u32) -> Duration {
        let exponent = completed_attempts.saturating_sub(1).min(16);
        let multiplier = 1u64 << exponent;
        Duration::from_millis(
            self.config
                .initial_retry_delay_ms
                .saturating_mul(multiplier),
        )
    }
}

enum CursorDisposition {
    Replay,
    Current,
}

fn decode_change(diff: &ResultDiff) -> Result<Option<RowChange>> {
    let (value, signature, deleted) = match diff {
        ResultDiff::Add {
            data,
            row_signature,
        } => (data, *row_signature, false),
        ResultDiff::Delete {
            data,
            row_signature,
        } => (data, *row_signature, true),
        ResultDiff::Update {
            before,
            after,
            row_signature,
            ..
        } => {
            let before: CapacityRow = serde_json::from_value(before.clone())
                .context("capacity update 'before' row violates the strict contract")?;
            before.validate()?;
            let after_row: CapacityRow = serde_json::from_value(after.clone())
                .context("capacity update 'after' row violates the strict contract")?;
            ensure!(
                before.worker_id == after_row.worker_id
                    && before.repository_owner == after_row.repository_owner
                    && before.repository_name == after_row.repository_name,
                "capacity update cannot change its worker or repository identity"
            );
            return Ok(Some(RowChange {
                row: after_row,
                row_signature: *row_signature,
                deleted: false,
            }));
        }
        ResultDiff::Aggregation {
            before,
            after,
            row_signature,
        } => {
            let after_row: CapacityRow = serde_json::from_value(after.clone())
                .context("capacity aggregation 'after' row violates the strict contract")?;
            if let Some(before) = before {
                let before: CapacityRow = serde_json::from_value(before.clone())
                    .context("capacity aggregation 'before' row violates the strict contract")?;
                before.validate()?;
                ensure!(
                    before.worker_id == after_row.worker_id
                        && before.repository_owner == after_row.repository_owner
                        && before.repository_name == after_row.repository_name,
                    "capacity aggregation cannot change its worker or repository identity"
                );
            }
            return Ok(Some(RowChange {
                row: after_row,
                row_signature: *row_signature,
                deleted: false,
            }));
        }
        ResultDiff::Noop => return Ok(None),
    };
    let row = serde_json::from_value(value.clone())
        .context("capacity row violates the strict contract")?;
    Ok(Some(RowChange {
        row,
        row_signature: signature,
        deleted,
    }))
}

fn canonical_time(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn bounded_error(reason: &str) -> String {
    const MAX_ERROR_BYTES: usize = 1024;
    if reason.len() <= MAX_ERROR_BYTES {
        return reason.to_string();
    }
    let mut end = MAX_ERROR_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &reason[..end])
}
