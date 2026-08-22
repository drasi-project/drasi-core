// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::agents::{
    AgentDefinition, AgentFile, AgentFileContent, AgentFileLocation, MAX_AGENT_SLOTS,
};
use crate::mapping::{agent_changes, allocation_changes, set_artifact_trusted, AgentProjection};
use crate::workgraph::{slot_id, Outcome, TaskType};
use anyhow::{Context, Result as AnyResult};
use chrono::{DateTime, SecondsFormat, Utc};
use drasi_core::models::SourceChange;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::{WalError, WalProvider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::Mutex;

const VERSION: u8 = 2;
const STATE_KEY: &str = "allocator:state";
const DELIVERY_PREFIX: &str = "delivery:";

#[derive(Clone, Debug)]
pub enum AllocationEvent {
    TaskCancelled {
        task_node_id: String,
    },
    Comment {
        comment_node_id: String,
        task_node_id: String,
        artifact: Option<AllocationArtifact>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllocationArtifact {
    Assignment {
        trusted: bool,
        task_type: TaskType,
        agent_id: String,
        created_at: String,
    },
    Result {
        reporter_trusted: bool,
        task_type: TaskType,
        lease_id: String,
        outcome: Outcome,
        body_digest: String,
    },
    Feedback {
        reporter_trusted: bool,
        result_comment_node_id: String,
        result_body_digest: String,
        body_digest: String,
    },
    Acceptance {
        reporter_trusted: bool,
        result_comment_node_id: String,
        result_body_digest: String,
        body_digest: String,
    },
}

#[derive(Clone, Debug, Default)]
pub struct AllocationDelta {
    pub trusted: bool,
    pub ended: Vec<ActiveLease>,
    pub started: Vec<ActiveLease>,
    pub removed_slots: BTreeSet<(String, u32)>,
    pub removed_agents: BTreeSet<String>,
    pub affected_agents: BTreeSet<String>,
    pub untrusted_assignments: BTreeSet<String>,
    pub untrusted_feedback: BTreeSet<String>,
    pub untrusted_acceptances: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveLease {
    pub lease_id: String,
    pub task_node_id: String,
    pub assignment_comment_node_id: String,
    pub agent_id: String,
    pub slot_id: String,
    pub slot_number: u32,
    pub task_type: TaskType,
    pub acquired_at: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRuntime {
    pub configured: bool,
    pub configured_slots: u32,
    pub queue_depth: usize,
    pub active_lease_count: usize,
    pub available_slot_count: usize,
    pub retiring_slots: BTreeSet<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentState {
    agent_id: String,
    configured: bool,
    configured_slots: u32,
    lease_duration_seconds: i64,
    retiring_slots: BTreeSet<u32>,
}

impl AgentState {
    fn new(agent: &AgentDefinition) -> Self {
        Self {
            agent_id: agent.agent_id.clone(),
            configured: true,
            configured_slots: agent.slots,
            lease_duration_seconds: agent.lease_duration_seconds,
            retiring_slots: BTreeSet::new(),
        }
    }

    fn slots(&self) -> BTreeSet<u32> {
        (1..=self.configured_slots)
            .chain(self.retiring_slots.iter().copied())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueEntry {
    task_node_id: String,
    task_type: TaskType,
    agent_id: String,
    created_at: String,
    next_attempt: u64,
    eligible: bool,
    queued_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum StoredArtifact {
    Assignment {
        task_node_id: String,
    },
    Result {
        task_node_id: String,
        assignment_comment_node_id: String,
        lease_id: String,
        task_type: TaskType,
        outcome: Outcome,
        body_digest: String,
    },
    Feedback {
        task_node_id: String,
        result_comment_node_id: String,
        result_body_digest: String,
        body_digest: String,
        applied: bool,
    },
    Acceptance {
        task_node_id: String,
        result_comment_node_id: String,
        result_body_digest: String,
        body_digest: String,
    },
}

impl StoredArtifact {
    fn task(&self) -> &str {
        match self {
            Self::Assignment { task_node_id }
            | Self::Result { task_node_id, .. }
            | Self::Feedback { task_node_id, .. }
            | Self::Acceptance { task_node_id, .. } => task_node_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationState {
    version: u8,
    agents: BTreeMap<String, AgentState>,
    queue: BTreeMap<String, QueueEntry>,
    assignment_attempts: BTreeMap<String, u64>,
    comments: BTreeMap<String, StoredArtifact>,
    active: BTreeMap<String, ActiveLease>,
    pub pending: Vec<SourceChange>,
}

pub struct Allocator {
    source_id: String,
    store: Arc<dyn StateStoreProvider>,
    wal: Arc<dyn WalProvider>,
    gate: Mutex<()>,
}

impl Allocator {
    pub fn new(
        source_id: String,
        store: Arc<dyn StateStoreProvider>,
        wal: Arc<dyn WalProvider>,
    ) -> Self {
        Self {
            source_id,
            store,
            wal,
            gate: Mutex::new(()),
        }
    }

    pub async fn recover(&self, effective_from: u64) -> AnyResult<usize> {
        let _guard = self.gate.lock().await;
        let mut state = self.ready_state().await?;
        let delta = state.restatement();
        let changes = allocation_changes(
            &self.source_id,
            effective_from,
            &delta,
            &state.agent_runtime(),
        );
        self.commit(&mut state, changes).await
    }

    pub async fn ingest(
        &self,
        delivery_id: &str,
        event: AllocationEvent,
        mut changes: Vec<SourceChange>,
        effective_from: u64,
    ) -> AnyResult<(usize, bool)> {
        let _guard = self.gate.lock().await;
        let key = format!("{DELIVERY_PREFIX}{delivery_id}");
        if self.store.contains_key(&self.source_id, &key).await? {
            return Ok((0, false));
        }
        let mut state = self.ready_state().await?;
        let comment = match &event {
            AllocationEvent::Comment {
                comment_node_id, ..
            } => Some(comment_node_id.clone()),
            _ => None,
        };
        let cancel = matches!(event, AllocationEvent::TaskCancelled { .. });
        let delta = state.apply(event, Utc::now());
        if let Some(comment) = comment {
            set_artifact_trusted(&mut changes, &comment, delta.trusted);
        }
        let synthetic = allocation_changes(
            &self.source_id,
            effective_from,
            &delta,
            &state.agent_runtime(),
        );
        let appended = if cancel {
            self.commit(&mut state, synthetic).await? + self.append(&changes).await?
        } else {
            let ordinary = self.append(&changes).await?;
            ordinary + self.commit(&mut state, synthetic).await?
        };
        self.store.set(&self.source_id, &key, Vec::new()).await?;
        Ok((appended, delta.trusted))
    }

    pub async fn append_delivery(
        &self,
        delivery_id: &str,
        changes: &[SourceChange],
    ) -> AnyResult<usize> {
        let _guard = self.gate.lock().await;
        let key = format!("{DELIVERY_PREFIX}{delivery_id}");
        if self.store.contains_key(&self.source_id, &key).await? {
            return Ok(0);
        }
        let mut state = self.ready_state().await?;
        let appended = self.append(changes).await?;
        self.store.set(&self.source_id, &key, Vec::new()).await?;
        if !state.pending.is_empty() {
            state.pending.clear();
            self.save(&state).await?;
        }
        Ok(appended)
    }

    pub async fn completed(&self, delivery_id: &str) -> AnyResult<bool> {
        let _guard = self.gate.lock().await;
        self.store
            .contains_key(&self.source_id, &format!("{DELIVERY_PREFIX}{delivery_id}"))
            .await
            .map_err(Into::into)
    }

    pub async fn mark_completed(&self, delivery_id: &str) -> AnyResult<()> {
        let _guard = self.gate.lock().await;
        self.store
            .set(
                &self.source_id,
                &format!("{DELIVERY_PREFIX}{delivery_id}"),
                Vec::new(),
            )
            .await
            .map_err(Into::into)
    }

    pub async fn sync_agents(
        &self,
        location: &AgentFileLocation,
        file: &AgentFile,
        content: &AgentFileContent,
        effective_from: u64,
    ) -> AnyResult<usize> {
        let _guard = self.gate.lock().await;
        let mut state = self.ready_state().await?;
        let delta = state.sync_agents(file, Utc::now());
        let mut changes = agent_changes(
            &self.source_id,
            effective_from,
            location,
            &AgentProjection::Loaded { file, content },
            &state.retiring_slots(),
            &BTreeMap::new(),
        );
        changes.extend(allocation_changes(
            &self.source_id,
            effective_from,
            &delta,
            &state.agent_runtime(),
        ));
        self.commit(&mut state, changes).await
    }

    pub async fn append_projection(&self, changes: &[SourceChange]) -> AnyResult<usize> {
        let _guard = self.gate.lock().await;
        self.ready_state().await?;
        self.append(changes).await
    }

    pub async fn expire(&self, now: DateTime<Utc>, effective_from: u64) -> AnyResult<usize> {
        let _guard = self.gate.lock().await;
        let mut state = self.ready_state().await?;
        let delta = state.expire(now);
        let changes = allocation_changes(
            &self.source_id,
            effective_from,
            &delta,
            &state.agent_runtime(),
        );
        if changes.is_empty() {
            return Ok(0);
        }
        self.commit(&mut state, changes).await
    }

    pub async fn validate_active(
        &self,
        task: &str,
        lease: &str,
        assignment: &str,
        agent: &str,
        slot: &str,
        now: DateTime<Utc>,
    ) -> AnyResult<Option<ActiveLease>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state
            .active_exact(task, lease, assignment, agent, slot, now)
            .cloned())
    }

    async fn ready_state(&self) -> AnyResult<AllocationState> {
        let mut state = match self.store.get(&self.source_id, STATE_KEY).await? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .context("allocator state is corrupt or has an unsupported schema")?,
            None => AllocationState::default(),
        };
        state.validate().map_err(anyhow::Error::msg)?;
        if !state.pending.is_empty() {
            self.append(&state.pending).await?;
            state.pending.clear();
            self.save(&state).await?;
        }
        Ok(state)
    }

    async fn commit(
        &self,
        state: &mut AllocationState,
        changes: Vec<SourceChange>,
    ) -> AnyResult<usize> {
        state
            .validate()
            .map_err(anyhow::Error::msg)
            .context("allocator transition produced invalid state")?;
        state.pending = changes;
        self.save(state).await?;
        let appended = self.append(&state.pending).await?;
        state.pending.clear();
        self.save(state).await?;
        Ok(appended)
    }

    async fn append(&self, changes: &[SourceChange]) -> AnyResult<usize> {
        for change in changes {
            self.wal
                .append(&self.source_id, change)
                .await
                .map_err(|error| match error {
                    WalError::CapacityExhausted(message) => {
                        anyhow::anyhow!("source WAL capacity exhausted: {message}")
                    }
                    error => anyhow::anyhow!("source WAL append failed: {error}"),
                })?;
        }
        Ok(changes.len())
    }

    async fn save(&self, state: &AllocationState) -> AnyResult<()> {
        self.store
            .set(
                &self.source_id,
                STATE_KEY,
                serde_json::to_vec(state).context("failed to encode allocator state")?,
            )
            .await
            .context("failed to persist allocator state")
    }
}

impl Default for AllocationState {
    fn default() -> Self {
        Self {
            version: VERSION,
            agents: BTreeMap::new(),
            queue: BTreeMap::new(),
            assignment_attempts: BTreeMap::new(),
            comments: BTreeMap::new(),
            active: BTreeMap::new(),
            pending: Vec::new(),
        }
    }
}

impl AllocationState {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!("allocator state version must equal {VERSION}"));
        }
        let mut tasks = BTreeSet::new();
        let mut slots = BTreeSet::new();
        for (id, lease) in &self.active {
            let assignment = self
                .queue
                .get(&lease.assignment_comment_node_id)
                .ok_or("active Lease has no Assignment")?;
            let agent = self
                .agents
                .get(&lease.agent_id)
                .ok_or("active Lease has no agent")?;
            if id != &lease.lease_id
                || assignment.task_node_id != lease.task_node_id
                || assignment.agent_id != lease.agent_id
                || assignment.task_type != lease.task_type
                || assignment.eligible
                || assignment.queued_by.is_some()
                || assignment.next_attempt == 0
                || id
                    != &make_lease_id(
                        &lease.task_node_id,
                        &lease.assignment_comment_node_id,
                        assignment.next_attempt,
                    )
                || !matches!(
                    self.comments.get(&lease.assignment_comment_node_id),
                    Some(StoredArtifact::Assignment { task_node_id })
                        if task_node_id == &lease.task_node_id
                )
                || lease.slot_id != slot_id(&lease.agent_id, lease.slot_number)
                || (lease.slot_number > agent.configured_slots
                    && !agent.retiring_slots.contains(&lease.slot_number))
                || !tasks.insert(&lease.task_node_id)
                || !slots.insert(&lease.slot_id)
            {
                return Err("allocator state violates active Lease invariants".into());
            }
            let acquired =
                DateTime::parse_from_rfc3339(&lease.acquired_at).map_err(|e| e.to_string())?;
            let expires =
                DateTime::parse_from_rfc3339(&lease.expires_at).map_err(|e| e.to_string())?;
            if acquired >= expires {
                return Err("active Lease acquiredAt must precede expiresAt".into());
            }
        }
        for (id, entry) in &self.queue {
            if !matches!(
                self.comments.get(id),
                Some(StoredArtifact::Assignment { task_node_id })
                    if task_node_id == &entry.task_node_id
            ) || self
                .assignment_attempts
                .get(id)
                .copied()
                .unwrap_or_default()
                != entry.next_attempt
                || self.agents.get(&entry.agent_id).is_none_or(|agent| {
                    !agent.configured
                        && !self
                            .active
                            .values()
                            .any(|lease| lease.assignment_comment_node_id.as_str() == id.as_str())
                })
            {
                return Err("allocator queue entry has no matching Assignment".into());
            }
        }
        for artifact in self.comments.values() {
            match artifact {
                StoredArtifact::Feedback {
                    task_node_id,
                    result_comment_node_id,
                    result_body_digest,
                    ..
                }
                | StoredArtifact::Acceptance {
                    task_node_id,
                    result_comment_node_id,
                    result_body_digest,
                    ..
                } if !self.result_matches(
                    task_node_id,
                    result_comment_node_id,
                    result_body_digest,
                ) =>
                {
                    return Err(
                        "allocator Feedback or Acceptance has no matching current Result".into(),
                    );
                }
                _ => {}
            }
        }
        if self.agents.iter().any(|(id, agent)| {
            id != &agent.agent_id
                || agent.lease_duration_seconds <= 0
                || (agent.configured && agent.configured_slots == 0)
                || (!agent.configured && agent.configured_slots != 0)
                || agent.configured_slots > MAX_AGENT_SLOTS
                || agent
                    .retiring_slots
                    .iter()
                    .any(|slot| *slot == 0 || *slot <= agent.configured_slots)
        }) {
            return Err("allocator state contains an invalid agent snapshot".into());
        }
        Ok(())
    }

    pub fn apply(&mut self, event: AllocationEvent, now: DateTime<Utc>) -> AllocationDelta {
        let mut delta = AllocationDelta::default();
        match event {
            AllocationEvent::TaskCancelled { task_node_id } => {
                let ids: Vec<(String, bool, bool, bool)> = self
                    .comments
                    .iter()
                    .filter(|(_, artifact)| artifact.task() == task_node_id)
                    .map(|(id, artifact)| {
                        (
                            id.clone(),
                            matches!(artifact, StoredArtifact::Assignment { .. }),
                            matches!(artifact, StoredArtifact::Feedback { .. }),
                            matches!(artifact, StoredArtifact::Acceptance { .. }),
                        )
                    })
                    .collect();
                for (id, assignment, feedback, acceptance) in ids {
                    self.retract(&id, &mut delta);
                    if assignment {
                        delta.untrusted_assignments.insert(id.clone());
                    }
                    if feedback {
                        delta.untrusted_feedback.insert(id.clone());
                    }
                    if acceptance {
                        delta.untrusted_acceptances.insert(id);
                    }
                }
            }
            AllocationEvent::Comment {
                comment_node_id,
                task_node_id,
                artifact,
            } => self.apply_comment(&comment_node_id, &task_node_id, artifact, &mut delta),
        }
        self.allocate(now, &mut delta);
        delta
    }

    pub fn expire(&mut self, now: DateTime<Utc>) -> AllocationDelta {
        let now_text = timestamp(now);
        let mut expired: Vec<_> = self
            .active
            .values()
            .filter(|lease| lease.expires_at.as_str() <= now_text.as_str())
            .map(|lease| (lease.expires_at.clone(), lease.lease_id.clone()))
            .collect();
        expired.sort();
        let mut delta = AllocationDelta::default();
        for (_, lease_id) in expired {
            let assignment_id = self
                .active
                .get(&lease_id)
                .map(|lease| lease.assignment_comment_node_id.clone());
            if let Some((id, assignment)) =
                assignment_id.and_then(|id| self.queue.get_mut(&id).map(|entry| (id, entry)))
            {
                assignment.eligible = true;
                assignment.queued_by = Some(id);
            }
            self.release(&lease_id, &mut delta);
        }
        self.allocate(now, &mut delta);
        delta
    }

    pub fn sync_agents(&mut self, file: &AgentFile, now: DateTime<Utc>) -> AllocationDelta {
        let old: BTreeMap<_, _> = self
            .agents
            .iter()
            .map(|(id, agent)| (id.clone(), agent.slots()))
            .collect();
        let configured: BTreeSet<_> = file
            .agents
            .iter()
            .map(|agent| agent.agent_id.clone())
            .collect();
        for definition in &file.agents {
            let active = self.active_slots(&definition.agent_id);
            let mut agent = AgentState::new(definition);
            agent.retiring_slots = active
                .into_iter()
                .filter(|slot| *slot > definition.slots)
                .collect();
            self.agents.insert(definition.agent_id.clone(), agent);
        }
        for id in self
            .agents
            .keys()
            .filter(|id| !configured.contains(*id))
            .cloned()
            .collect::<Vec<_>>()
        {
            let active = self.active_slots(&id);
            if active.is_empty() {
                self.agents.remove(&id);
            } else if let Some(agent) = self.agents.get_mut(&id) {
                agent.configured = false;
                agent.configured_slots = 0;
                agent.retiring_slots = active;
            }
        }
        let mut delta = AllocationDelta::default();
        let active_assignments: BTreeSet<_> = self
            .active
            .values()
            .map(|lease| lease.assignment_comment_node_id.clone())
            .collect();
        for assignment_id in self
            .queue
            .iter()
            .filter(|(assignment_id, entry)| {
                !configured.contains(&entry.agent_id)
                    && !active_assignments.contains(*assignment_id)
            })
            .map(|(assignment_id, _)| assignment_id.clone())
            .collect::<Vec<_>>()
        {
            if let Some(entry) = self.queue.remove(&assignment_id) {
                self.comments.remove(&assignment_id);
                delta.affected_agents.insert(entry.agent_id);
                delta.untrusted_assignments.insert(assignment_id);
            }
        }
        for id in old
            .keys()
            .chain(self.agents.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
        {
            let next = self
                .agents
                .get(&id)
                .map(AgentState::slots)
                .unwrap_or_default();
            for slot in old
                .get(&id)
                .into_iter()
                .flatten()
                .filter(|slot| !next.contains(slot))
            {
                delta.removed_slots.insert((id.clone(), *slot));
            }
            if !self.agents.contains_key(&id) {
                delta.removed_agents.insert(id.clone());
            }
            delta.affected_agents.insert(id);
        }
        self.allocate(now, &mut delta);
        delta
    }

    pub fn active_exact(
        &self,
        task: &str,
        lease: &str,
        assignment: &str,
        agent: &str,
        slot: &str,
        now: DateTime<Utc>,
    ) -> Option<&ActiveLease> {
        let now = timestamp(now);
        self.active.get(lease).filter(|active| {
            active.task_node_id == task
                && active.assignment_comment_node_id == assignment
                && active.agent_id == agent
                && active.slot_id == slot
                && active.expires_at > now
        })
    }

    pub fn active_leases(&self) -> impl Iterator<Item = &ActiveLease> {
        self.active.values()
    }

    pub fn agent_runtime(&self) -> BTreeMap<String, AgentRuntime> {
        self.agents
            .iter()
            .map(|(id, agent)| {
                let occupied = self.active_slots(id);
                let queue_depth = self
                    .queue
                    .values()
                    .filter(|entry| {
                        entry.agent_id == *id
                            && entry.eligible
                            && !self.task_active(&entry.task_node_id)
                    })
                    .count();
                let available = agent.configured.then(|| {
                    (1..=agent.configured_slots)
                        .filter(|slot| !occupied.contains(slot))
                        .count()
                });
                (
                    id.clone(),
                    AgentRuntime {
                        configured: agent.configured,
                        configured_slots: agent.configured_slots,
                        queue_depth,
                        active_lease_count: occupied.len(),
                        available_slot_count: available.unwrap_or(0),
                        retiring_slots: agent.retiring_slots.clone(),
                    },
                )
            })
            .collect()
    }

    fn restatement(&self) -> AllocationDelta {
        AllocationDelta {
            started: self.active.values().cloned().collect(),
            affected_agents: self.agents.keys().cloned().collect(),
            ..AllocationDelta::default()
        }
    }

    fn retiring_slots(&self) -> BTreeMap<String, BTreeSet<u32>> {
        self.agents
            .iter()
            .map(|(id, agent)| (id.clone(), agent.retiring_slots.clone()))
            .collect()
    }

    fn apply_comment(
        &mut self,
        id: &str,
        task: &str,
        artifact: Option<AllocationArtifact>,
        delta: &mut AllocationDelta,
    ) {
        if self.same_current(id, task, artifact.as_ref()) {
            delta.trusted = true;
            return;
        }
        self.retract(id, delta);
        match artifact {
            Some(AllocationArtifact::Assignment {
                trusted: true,
                task_type,
                agent_id,
                created_at,
            }) if self
                .agents
                .get(&agent_id)
                .is_some_and(|agent| agent.configured) =>
            {
                let next_attempt = self
                    .assignment_attempts
                    .get(id)
                    .copied()
                    .unwrap_or_default();
                self.comments.insert(
                    id.into(),
                    StoredArtifact::Assignment {
                        task_node_id: task.into(),
                    },
                );
                self.queue.insert(
                    id.into(),
                    QueueEntry {
                        task_node_id: task.into(),
                        task_type,
                        agent_id: agent_id.clone(),
                        created_at,
                        next_attempt,
                        eligible: true,
                        queued_by: Some(id.into()),
                    },
                );
                delta.affected_agents.insert(agent_id);
                delta.trusted = true;
            }
            Some(AllocationArtifact::Result {
                reporter_trusted: true,
                task_type,
                lease_id,
                outcome,
                body_digest,
            }) => {
                let matched = self
                    .active
                    .get(&lease_id)
                    .filter(|lease| lease.task_node_id == task && lease.task_type == task_type)
                    .cloned();
                if let Some(lease) = matched {
                    if let Some(entry) = self.queue.get_mut(&lease.assignment_comment_node_id) {
                        entry.eligible = false;
                        entry.queued_by = None;
                    }
                    self.release(&lease_id, delta);
                    self.comments.insert(
                        id.into(),
                        StoredArtifact::Result {
                            task_node_id: task.into(),
                            assignment_comment_node_id: lease.assignment_comment_node_id,
                            lease_id,
                            task_type,
                            outcome,
                            body_digest,
                        },
                    );
                    delta.trusted = true;
                }
            }
            Some(AllocationArtifact::Feedback {
                reporter_trusted: true,
                result_comment_node_id,
                result_body_digest,
                body_digest,
            }) if self.result_matches(task, &result_comment_node_id, &result_body_digest) => {
                self.comments.insert(
                    id.into(),
                    StoredArtifact::Feedback {
                        task_node_id: task.into(),
                        result_comment_node_id,
                        result_body_digest,
                        body_digest,
                        applied: false,
                    },
                );
                self.apply_feedback(id, delta);
                delta.trusted = true;
            }
            Some(AllocationArtifact::Acceptance {
                reporter_trusted: true,
                result_comment_node_id,
                result_body_digest,
                body_digest,
            }) if self.result_matches(task, &result_comment_node_id, &result_body_digest) => {
                self.comments.insert(
                    id.into(),
                    StoredArtifact::Acceptance {
                        task_node_id: task.into(),
                        result_comment_node_id,
                        result_body_digest,
                        body_digest,
                    },
                );
                self.suppress(task, delta);
                delta.trusted = true;
            }
            _ => {}
        }
    }

    fn same_current(&self, id: &str, task: &str, artifact: Option<&AllocationArtifact>) -> bool {
        match (self.comments.get(id), artifact) {
            (
                Some(StoredArtifact::Assignment { task_node_id }),
                Some(AllocationArtifact::Assignment {
                    trusted: true,
                    task_type,
                    agent_id,
                    created_at,
                }),
            ) => {
                task_node_id == task
                    && self.queue.get(id).is_some_and(|entry| {
                        entry.task_type == *task_type
                            && entry.agent_id == *agent_id
                            && entry.created_at == *created_at
                            && self
                                .agents
                                .get(agent_id)
                                .is_some_and(|agent| agent.configured)
                    })
            }
            (
                Some(StoredArtifact::Result {
                    task_node_id,
                    lease_id: stored_lease,
                    task_type: stored_type,
                    outcome: stored_outcome,
                    body_digest: stored_digest,
                    ..
                }),
                Some(AllocationArtifact::Result {
                    reporter_trusted: true,
                    task_type,
                    lease_id,
                    outcome,
                    body_digest,
                }),
            ) => {
                task_node_id == task
                    && stored_lease == lease_id
                    && stored_type == task_type
                    && stored_outcome == outcome
                    && stored_digest == body_digest
            }
            (
                Some(StoredArtifact::Feedback {
                    task_node_id,
                    result_comment_node_id: result,
                    result_body_digest: digest,
                    body_digest: body,
                    ..
                }),
                Some(AllocationArtifact::Feedback {
                    reporter_trusted: true,
                    result_comment_node_id,
                    result_body_digest,
                    body_digest,
                }),
            ) => {
                task_node_id == task
                    && result == result_comment_node_id
                    && digest == result_body_digest
                    && body == body_digest
                    && self.result_matches(task, result, digest)
            }
            (
                Some(StoredArtifact::Acceptance {
                    task_node_id,
                    result_comment_node_id: result,
                    result_body_digest: digest,
                    body_digest: body,
                }),
                Some(AllocationArtifact::Acceptance {
                    reporter_trusted: true,
                    result_comment_node_id,
                    result_body_digest,
                    body_digest,
                }),
            ) => {
                task_node_id == task
                    && result == result_comment_node_id
                    && digest == result_body_digest
                    && body == body_digest
                    && self.result_matches(task, result, digest)
            }
            _ => false,
        }
    }

    fn retract(&mut self, id: &str, delta: &mut AllocationDelta) {
        let Some(artifact) = self.comments.remove(id) else {
            return;
        };
        match artifact {
            StoredArtifact::Assignment { .. } => {
                if let Some(entry) = self.queue.remove(id) {
                    for lease in self
                        .active
                        .values()
                        .filter(|lease| lease.assignment_comment_node_id == id)
                        .map(|lease| lease.lease_id.clone())
                        .collect::<Vec<_>>()
                    {
                        self.release(&lease, delta);
                    }
                    delta.affected_agents.insert(entry.agent_id);
                }
            }
            StoredArtifact::Feedback { task_node_id, .. } => {
                for entry in self
                    .queue
                    .values_mut()
                    .filter(|entry| entry.queued_by.as_deref() == Some(id))
                {
                    entry.eligible = false;
                    entry.queued_by = None;
                    delta.affected_agents.insert(entry.agent_id.clone());
                }
                self.apply_waiting(&task_node_id, delta);
            }
            StoredArtifact::Acceptance { task_node_id, .. } => {
                self.apply_waiting(&task_node_id, delta);
            }
            StoredArtifact::Result { .. } => {
                let dependents = self
                    .comments
                    .iter()
                    .filter_map(|(dependent_id, artifact)| match artifact {
                        StoredArtifact::Feedback {
                            result_comment_node_id,
                            ..
                        } if result_comment_node_id == id => Some((dependent_id.clone(), false)),
                        StoredArtifact::Acceptance {
                            result_comment_node_id,
                            ..
                        } if result_comment_node_id == id => Some((dependent_id.clone(), true)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                for (dependent_id, acceptance) in dependents {
                    self.retract(&dependent_id, delta);
                    if acceptance {
                        delta.untrusted_acceptances.insert(dependent_id);
                    } else {
                        delta.untrusted_feedback.insert(dependent_id);
                    }
                }
            }
        }
    }

    fn apply_feedback(&mut self, id: &str, delta: &mut AllocationDelta) {
        let Some(StoredArtifact::Feedback {
            result_comment_node_id,
            result_body_digest,
            applied: false,
            ..
        }) = self.comments.get(id)
        else {
            return;
        };
        let (result, digest) = (result_comment_node_id.clone(), result_body_digest.clone());
        if self.accepted(&result, &digest) {
            return;
        }
        let assignment = match self.comments.get(&result) {
            Some(StoredArtifact::Result {
                assignment_comment_node_id,
                ..
            }) => assignment_comment_node_id.clone(),
            _ => return,
        };
        if self
            .active
            .values()
            .any(|lease| lease.assignment_comment_node_id == assignment)
        {
            if let Some(StoredArtifact::Feedback { applied, .. }) = self.comments.get_mut(id) {
                *applied = true;
            }
            return;
        }
        if let Some(entry) = self.queue.get_mut(&assignment) {
            entry.eligible = true;
            entry.queued_by = Some(id.into());
            delta.affected_agents.insert(entry.agent_id.clone());
            if let Some(StoredArtifact::Feedback { applied, .. }) = self.comments.get_mut(id) {
                *applied = true;
            }
        }
    }

    fn apply_waiting(&mut self, task: &str, delta: &mut AllocationDelta) {
        for id in self
            .comments
            .iter()
            .filter(|(_, artifact)| {
                matches!(artifact, StoredArtifact::Feedback { task_node_id, applied: false, .. } if task_node_id == task)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
        {
            self.apply_feedback(&id, delta);
        }
    }

    fn suppress(&mut self, task: &str, delta: &mut AllocationDelta) {
        for artifact in self.comments.values_mut() {
            if let StoredArtifact::Feedback {
                task_node_id,
                applied,
                ..
            } = artifact
            {
                if task_node_id == task {
                    *applied = false;
                }
            }
        }
        for entry in self.queue.values_mut() {
            if entry.task_node_id == task {
                entry.eligible = false;
                entry.queued_by = None;
                delta.affected_agents.insert(entry.agent_id.clone());
            }
        }
        let active: Vec<_> = self
            .active
            .values()
            .filter(|lease| lease.task_node_id == task)
            .map(|lease| lease.lease_id.clone())
            .collect();
        for lease in active {
            self.release(&lease, delta);
        }
    }

    fn result_matches(&self, task: &str, result: &str, digest: &str) -> bool {
        matches!(
            self.comments.get(result),
            Some(StoredArtifact::Result { task_node_id, body_digest, .. })
                if task_node_id == task && body_digest == digest
        )
    }

    fn accepted(&self, result: &str, digest: &str) -> bool {
        self.comments.values().any(|artifact| {
            matches!(
                artifact,
                StoredArtifact::Acceptance {
                    result_comment_node_id,
                    result_body_digest,
                    ..
                } if result_comment_node_id == result && result_body_digest == digest
            )
        })
    }

    fn allocate(&mut self, now: DateTime<Utc>, delta: &mut AllocationDelta) {
        let mut slots: Vec<_> = self
            .agents
            .iter()
            .filter(|(_, agent)| agent.configured)
            .flat_map(|(id, agent)| {
                (1..=agent.configured_slots)
                    .map(move |number| (number, slot_id(id, number), id.clone()))
            })
            .collect();
        slots.sort();
        for (slot_number, slot, agent_id) in slots {
            let agent = self.agents[&agent_id].clone();
            if self.active.values().any(|lease| lease.slot_id == slot) {
                continue;
            }
            let mut queue: Vec<_> = self
                .queue
                .iter()
                .filter(|(_, entry)| {
                    entry.agent_id == agent_id
                        && entry.eligible
                        && !self.task_active(&entry.task_node_id)
                })
                .map(|(id, entry)| {
                    (
                        entry.created_at.clone(),
                        entry.task_node_id.clone(),
                        id.clone(),
                    )
                })
                .collect();
            queue.sort();
            let Some((_, _, assignment_id)) = queue.into_iter().next() else {
                continue;
            };
            let entry = self.queue.get_mut(&assignment_id).expect("entry exists");
            let attempt = self
                .assignment_attempts
                .entry(assignment_id.clone())
                .or_insert(entry.next_attempt);
            *attempt += 1;
            entry.next_attempt = *attempt;
            entry.eligible = false;
            entry.queued_by = None;
            let lease_id = make_lease_id(&entry.task_node_id, &assignment_id, entry.next_attempt);
            let lease = ActiveLease {
                lease_id: lease_id.clone(),
                task_node_id: entry.task_node_id.clone(),
                assignment_comment_node_id: assignment_id,
                agent_id: agent_id.clone(),
                slot_id: slot,
                slot_number,
                task_type: entry.task_type,
                acquired_at: timestamp(now),
                expires_at: timestamp(
                    now + chrono::Duration::seconds(agent.lease_duration_seconds),
                ),
            };
            self.active.insert(lease_id, lease.clone());
            delta.started.push(lease);
            delta.affected_agents.insert(agent_id.clone());
        }
    }

    fn release(&mut self, id: &str, delta: &mut AllocationDelta) {
        let Some(lease) = self.active.remove(id) else {
            return;
        };
        let assignment_id = lease.assignment_comment_node_id.clone();
        delta.affected_agents.insert(lease.agent_id.clone());
        delta.ended.push(lease.clone());
        if let Some(agent) = self
            .agents
            .get_mut(&lease.agent_id)
            .filter(|agent| agent.retiring_slots.contains(&lease.slot_number))
        {
            agent.retiring_slots.remove(&lease.slot_number);
            delta
                .removed_slots
                .insert((lease.agent_id.clone(), lease.slot_number));
            if !agent.configured && agent.retiring_slots.is_empty() {
                self.agents.remove(&lease.agent_id);
                delta.removed_agents.insert(lease.agent_id.clone());
            }
        }
        if self
            .agents
            .get(&lease.agent_id)
            .is_none_or(|agent| !agent.configured)
            && self.queue.remove(&assignment_id).is_some()
        {
            self.comments.remove(&assignment_id);
            delta.untrusted_assignments.insert(assignment_id);
        }
    }

    fn active_slots(&self, agent: &str) -> BTreeSet<u32> {
        self.active
            .values()
            .filter(|lease| lease.agent_id == agent)
            .map(|lease| lease.slot_number)
            .collect()
    }

    fn task_active(&self, task: &str) -> bool {
        self.active.values().any(|lease| lease.task_node_id == task)
    }
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn make_lease_id(task: &str, assignment: &str, attempt: u64) -> String {
    hex::encode(Sha256::digest(
        format!("{task}\0{assignment}\0{attempt}").as_bytes(),
    ))
}
