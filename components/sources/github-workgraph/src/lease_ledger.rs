// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::agents::{
    AgentDefinition, AgentFile, AgentFileContent, AgentFileLocation, MAX_AGENT_SLOTS,
};
use crate::mapping::{agent_changes, allocation_changes, AgentProjection};
use crate::model::slot_id;
use crate::protocol::{
    LifecycleArtifactDocument, PreparedProjectionCommit, ProjectionInput, RootIssueDocument,
    TaskDocument, WorkGraphAllocatorProjection, WorkGraphAssignmentBinding,
    WorkGraphDispatchBinding, WorkGraphProjector, WORKGRAPH_ASSIGN_MARKER,
    WORKGRAPH_DISPATCH_MARKER,
};
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

const VERSION: u8 = 8;
const STATE_KEY: &str = "allocator:state";
const DELIVERY_PREFIX: &str = "delivery:";
const WORKGRAPH_ORIGIN_PREFIX: &str = "workgraph-origin:";
const MAX_WORKGRAPH_ID_LENGTH: usize = 256;
const MAX_WORKGRAPH_PERMITTED_EXECUTORS: usize = 64;

fn workgraph_origin_key(origin_id: &str) -> String {
    let digest = Sha256::digest(origin_id.as_bytes());
    format!("{WORKGRAPH_ORIGIN_PREFIX}{}", hex::encode(digest))
}

#[derive(Clone, Debug, Default)]
pub struct AllocationDelta {
    pub removed_slots: BTreeSet<(String, u32)>,
    pub removed_agents: BTreeSet<String>,
    pub affected_agents: BTreeSet<String>,
    pub workgraph_ended: Vec<WorkGraphActiveLease>,
    pub workgraph_historical_ended: Vec<WorkGraphActiveLease>,
    pub workgraph_started: Vec<WorkGraphActiveLease>,
    pub workgraph_released: Vec<WorkGraphActiveLease>,
    pub workgraph_historical: Vec<WorkGraphActiveLease>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphActiveLease {
    pub lease_id: String,
    pub task_source_key: String,
    pub task_id: String,
    pub task_element_id: String,
    pub assignment_source_key: String,
    pub assignment_id: String,
    pub executor_id: String,
    pub slot_id: String,
    pub slot_number: u32,
    pub acquired_at: String,
    pub expires_at: String,
    pub has_dispatch: bool,
    pub completed: bool,
    pub completion_eligible: bool,
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
struct WorkGraphTaskState {
    task_id: String,
    task_element_id: String,
    is_open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkGraphAssignmentState {
    task_source_key: String,
    task_id: String,
    assignment_id: String,
    permitted_executors: Vec<String>,
    queued_at: u64,
    next_attempt: u64,
    eligible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationState {
    version: u8,
    agents: BTreeMap<String, AgentState>,
    #[serde(default)]
    workgraph_task_identities: BTreeMap<String, WorkGraphTaskState>,
    #[serde(default)]
    workgraph_assignments: BTreeMap<String, WorkGraphAssignmentState>,
    #[serde(default)]
    workgraph_assignment_attempts: BTreeMap<String, u64>,
    #[serde(default)]
    workgraph_active: BTreeMap<String, WorkGraphActiveLease>,
    #[serde(default)]
    workgraph_dispatched: BTreeMap<String, WorkGraphActiveLease>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_result_claims: BTreeMap<String, String>,
    pub pending: Vec<SourceChange>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pending_offset: usize,
    /// Opaque bounded checkpoint of the current WorkGraph projector state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    workgraph_checkpoint: Vec<u8>,
    /// Materialized task documents used to preserve parent linkage without
    /// parsing WorkGraph bodies or scanning projector history.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_tasks: BTreeMap<String, TaskDocument>,
    /// Root Issues currently carrying the exact WorkGraph admission label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_root_issues: BTreeMap<String, RootIssueDocument>,
    /// Latest authoritative GitHub revision observed for a Root Issue or task Issue.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_issue_revisions: BTreeMap<String, i64>,
    /// Current authenticated lifecycle documents used to validate directives
    /// emitted later when out-of-order dependencies become available.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_artifacts: BTreeMap<String, LifecycleArtifactDocument>,
    /// Origins staged with pending WAL changes but not yet finalized into
    /// their separate durable dedupe key.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pending_workgraph_origins: BTreeSet<String>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

pub struct Allocator {
    source_id: String,
    store: Arc<dyn StateStoreProvider>,
    wal: Arc<dyn WalProvider>,
    gate: Mutex<()>,
    pending_workgraph_commits: Mutex<BTreeMap<String, Box<dyn PreparedProjectionCommit>>>,
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
            pending_workgraph_commits: Mutex::new(BTreeMap::new()),
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

    pub async fn claim_active(
        &self,
        task_id: &str,
        lease_id: &str,
        assignment_id: &str,
        executor_id: &str,
        slot_id: &str,
        claim_id: &str,
        now: DateTime<Utc>,
    ) -> AnyResult<Option<WorkGraphActiveLease>> {
        let _guard = self.gate.lock().await;
        let mut state = self.ready_state().await?;
        let active = state
            .workgraph_active_exact(task_id, lease_id, assignment_id, executor_id, slot_id, now)
            .filter(|active| active.has_dispatch)
            .cloned();
        let Some(active) = active else {
            return Ok(None);
        };
        if state
            .workgraph_result_claims
            .get(lease_id)
            .is_some_and(|existing| existing != claim_id)
        {
            return Ok(None);
        }
        state
            .workgraph_result_claims
            .insert(lease_id.to_string(), claim_id.to_string());
        self.save(&state).await?;
        Ok(Some(active))
    }

    async fn ready_state(&self) -> AnyResult<AllocationState> {
        let mut state = match self.store.get(&self.source_id, STATE_KEY).await? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .context("allocator state is corrupt or has an unsupported schema")?,
            None => AllocationState::default(),
        };
        state.validate().map_err(anyhow::Error::msg)?;
        if !state.pending.is_empty() {
            self.append_pending(&mut state).await?;
            let commits = {
                let mut pending = self.pending_workgraph_commits.lock().await;
                std::mem::take(&mut *pending)
            };
            for (_, commit) in commits {
                commit.commit().await;
            }
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
        state.pending_offset = 0;
        self.save(state).await?;
        self.append_pending(state).await
    }

    async fn append_pending(&self, state: &mut AllocationState) -> AnyResult<usize> {
        let start = state.pending_offset;
        while state.pending_offset < state.pending.len() {
            self.wal
                .append(&self.source_id, &state.pending[state.pending_offset])
                .await
                .map_err(|error| match error {
                    WalError::CapacityExhausted(message) => {
                        anyhow::anyhow!("source WAL capacity exhausted: {message}")
                    }
                    error => anyhow::anyhow!("source WAL append failed: {error}"),
                })?;
            state.pending_offset += 1;
            self.save(state).await?;
        }
        let appended = state.pending_offset.saturating_sub(start);
        state.pending.clear();
        state.pending_offset = 0;
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

    // ── WorkGraph projection methods ──────────────────────────────────────────

    /// Process a WorkGraph projection input through the injected projector.
    ///
    /// Atomically stage the bounded projector checkpoint and pending changes,
    /// append the ordered WAL batch, then commit the prepared projector state.
    pub async fn ingest_workgraph(
        &self,
        projector: &dyn WorkGraphProjector,
        inputs: Vec<ProjectionInput>,
        effective_from: u64,
        origin_id: &str,
    ) -> AnyResult<(usize, Option<String>)> {
        let _guard = self.gate.lock().await;
        let mut state = self.ready_state().await?;
        let origin_key = workgraph_origin_key(origin_id);

        if self
            .store
            .contains_key(&self.source_id, &origin_key)
            .await?
        {
            if state.pending_workgraph_origins.remove(origin_id) {
                self.save(&state).await?;
            }
            return Ok((0, None));
        }
        if state.pending_workgraph_origins.contains(origin_id) {
            self.store
                .set(&self.source_id, &origin_key, Vec::new())
                .await?;
            state.pending_workgraph_origins.remove(origin_id);
            self.save(&state).await?;
            return Ok((0, None));
        }

        let mut accepted_inputs = inputs;
        let mut prepared = projector
            .prepare(accepted_inputs.clone(), effective_from)
            .await?;
        anyhow::ensure!(
            !prepared.checkpoint.is_empty(),
            "WorkGraph projector returned an empty recovery checkpoint"
        );
        let mut rejection = prepared.rejection.clone();
        let (mut next_root_issues, mut next_issue_revisions, mut next_tasks, mut next_artifacts) =
            stage_workgraph_documents(&state, &accepted_inputs);
        let mut candidate = state.clone();
        let transition =
            validate_workgraph_projection(&prepared.allocator, &next_tasks, &next_artifacts)
                .and_then(|()| {
                    candidate.reconcile_workgraph(
                        prepared.allocator.clone(),
                        &next_tasks,
                        effective_from,
                        Utc::now(),
                    )
                });
        let allocation_delta = match transition {
            Ok(delta) => delta,
            Err(error) => {
                let Some(fallback) = rejected_dispatch_retraction(&accepted_inputs) else {
                    return Err(error);
                };
                append_rejection(&mut rejection, Some(error.to_string()));
                accepted_inputs = fallback;
                prepared = projector
                    .prepare(accepted_inputs.clone(), effective_from)
                    .await?;
                anyhow::ensure!(
                    !prepared.checkpoint.is_empty(),
                    "WorkGraph projector returned an empty recovery checkpoint"
                );
                append_rejection(&mut rejection, prepared.rejection.clone());
                (
                    next_root_issues,
                    next_issue_revisions,
                    next_tasks,
                    next_artifacts,
                ) = stage_workgraph_documents(&state, &accepted_inputs);
                candidate = state.clone();
                validate_workgraph_projection(&prepared.allocator, &next_tasks, &next_artifacts)?;
                candidate.reconcile_workgraph(
                    prepared.allocator.clone(),
                    &next_tasks,
                    effective_from,
                    Utc::now(),
                )?
            }
        };
        state = candidate;
        let mut changes = prepared.changes;
        changes.extend(allocation_changes(
            &self.source_id,
            effective_from,
            &allocation_delta,
            &state.agent_runtime(),
        ));

        state.workgraph_tasks = next_tasks;
        state.workgraph_root_issues = next_root_issues;
        state.workgraph_issue_revisions = next_issue_revisions;
        state.workgraph_artifacts = next_artifacts;
        state.workgraph_checkpoint = prepared.checkpoint;
        state
            .pending_workgraph_origins
            .insert(origin_id.to_string());
        state.pending = changes;
        state.pending_offset = 0;
        self.save(&state).await?;
        self.pending_workgraph_commits
            .lock()
            .await
            .insert(origin_id.to_string(), prepared.commit);

        let appended = self.append_pending(&mut state).await?;

        let commit = self
            .pending_workgraph_commits
            .lock()
            .await
            .remove(origin_id)
            .expect("prepared WorkGraph commit accompanies a successful append");
        commit.commit().await;
        self.store
            .set(&self.source_id, &origin_key, Vec::new())
            .await?;
        state.pending_workgraph_origins.remove(origin_id);
        self.save(&state).await?;

        Ok((appended, rejection))
    }

    /// Return the bounded durable WorkGraph projector checkpoint.
    pub async fn workgraph_checkpoint(&self) -> AnyResult<Vec<u8>> {
        let _guard = self.gate.lock().await;
        let mut state = self.ready_state().await?;
        if !state.pending_workgraph_origins.is_empty() {
            for origin_id in &state.pending_workgraph_origins {
                self.store
                    .set(
                        &self.source_id,
                        &workgraph_origin_key(origin_id),
                        Vec::new(),
                    )
                    .await?;
            }
            state.pending_workgraph_origins.clear();
            self.save(&state).await?;
        }
        Ok(state.workgraph_checkpoint.clone())
    }

    /// Check if a WorkGraph origin ID was already processed.
    pub async fn workgraph_origin_completed(&self, origin_id: &str) -> AnyResult<bool> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        if state.pending_workgraph_origins.contains(origin_id) {
            return Ok(true);
        }
        self.store
            .contains_key(&self.source_id, &workgraph_origin_key(origin_id))
            .await
            .map_err(Into::into)
    }

    /// Return the latest accepted task document, preserving parent linkage
    /// across ordinary issue updates that do not carry sub-issue metadata.
    pub async fn latest_workgraph_task(&self, source_key: &str) -> AnyResult<Option<TaskDocument>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state.workgraph_tasks.get(source_key).cloned())
    }

    /// Return the currently admitted generation of a Root Issue.
    pub async fn latest_workgraph_root_issue(
        &self,
        source_key: &str,
    ) -> AnyResult<Option<RootIssueDocument>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state.workgraph_root_issues.get(source_key).cloned())
    }

    /// Return the latest authoritative GitHub revision observed for a Root Issue or task Issue.
    pub async fn latest_workgraph_issue_revision(
        &self,
        source_key: &str,
    ) -> AnyResult<Option<i64>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state.workgraph_issue_revisions.get(source_key).copied())
    }
}

fn stage_workgraph_documents(
    state: &AllocationState,
    inputs: &[ProjectionInput],
) -> (
    BTreeMap<String, RootIssueDocument>,
    BTreeMap<String, i64>,
    BTreeMap<String, TaskDocument>,
    BTreeMap<String, LifecycleArtifactDocument>,
) {
    let mut root_issues = state.workgraph_root_issues.clone();
    let mut issue_revisions = state.workgraph_issue_revisions.clone();
    let mut tasks = state.workgraph_tasks.clone();
    let mut artifacts = state.workgraph_artifacts.clone();
    apply_workgraph_documents(
        inputs,
        &mut root_issues,
        &mut issue_revisions,
        &mut tasks,
        &mut artifacts,
    );
    (root_issues, issue_revisions, tasks, artifacts)
}

fn append_rejection(rejection: &mut Option<String>, next: Option<String>) {
    let Some(next) = next.filter(|message| !message.is_empty()) else {
        return;
    };
    *rejection = Some(match rejection.take() {
        Some(existing) => format!("{existing}; {next}"),
        None => next,
    });
}

fn rejected_dispatch_retraction(inputs: &[ProjectionInput]) -> Option<Vec<ProjectionInput>> {
    let mut found_dispatch = false;
    let mut fallback = Vec::with_capacity(inputs.len());
    for input in inputs {
        match input {
            ProjectionInput::UpsertLifecycleArtifact(document)
                if document.body.starts_with(WORKGRAPH_DISPATCH_MARKER) =>
            {
                if found_dispatch {
                    return None;
                }
                found_dispatch = true;
                fallback.push(ProjectionInput::DeleteLifecycleArtifact {
                    source_key: document.source_key.clone(),
                });
            }
            ProjectionInput::RecordIssueRevision { .. } | ProjectionInput::UpsertLocator(_) => {
                fallback.push(input.clone());
            }
            _ => return None,
        }
    }
    found_dispatch.then_some(fallback)
}

fn apply_workgraph_documents(
    inputs: &[ProjectionInput],
    root_issues: &mut BTreeMap<String, RootIssueDocument>,
    issue_revisions: &mut BTreeMap<String, i64>,
    tasks: &mut BTreeMap<String, TaskDocument>,
    artifacts: &mut BTreeMap<String, LifecycleArtifactDocument>,
) {
    for input in inputs {
        match input {
            ProjectionInput::RecordIssueRevision {
                source_key,
                revision,
            } => {
                issue_revisions.insert(source_key.clone(), *revision);
            }
            ProjectionInput::UpsertRootIssue(document) => {
                root_issues.insert(document.source_key.clone(), document.clone());
            }
            ProjectionInput::DeleteRootIssue { source_key } => {
                root_issues.remove(source_key);
            }
            ProjectionInput::UpsertTask(document) => {
                tasks.insert(document.source_key.clone(), document.clone());
            }
            ProjectionInput::DeleteTask { source_key } => {
                tasks.remove(source_key);
            }
            ProjectionInput::UpsertLifecycleArtifact(document) => {
                artifacts.insert(document.source_key.clone(), document.clone());
            }
            ProjectionInput::DeleteLifecycleArtifact { source_key } => {
                artifacts.remove(source_key);
            }
            _ => {}
        }
    }
}

fn validate_workgraph_projection(
    projection: &WorkGraphAllocatorProjection,
    tasks: &BTreeMap<String, TaskDocument>,
    artifacts: &BTreeMap<String, LifecycleArtifactDocument>,
) -> AnyResult<()> {
    let task_bindings = projection
        .tasks
        .iter()
        .map(|task| (task.source_key.as_str(), task))
        .collect::<BTreeMap<_, _>>();
    anyhow::ensure!(
        task_bindings.len() == projection.tasks.len(),
        "WorkGraph allocator projection contains duplicate task source keys"
    );
    let mut task_ids = BTreeSet::new();
    let mut task_elements = BTreeSet::new();
    for task in &projection.tasks {
        anyhow::ensure!(
            tasks.contains_key(&task.source_key)
                && valid_workgraph_id(&task.source_key)
                && valid_workgraph_id(&task.task_id)
                && valid_workgraph_id(&task.task_element_id)
                && task_ids.insert(&task.task_id)
                && task_elements.insert(&task.task_element_id),
            "WorkGraph allocator projection contains an invalid or duplicate task binding"
        );
    }

    let assignments = projection
        .assignments
        .iter()
        .map(|assignment| (assignment.source_key.as_str(), assignment))
        .collect::<BTreeMap<_, _>>();
    anyhow::ensure!(
        assignments.len() == projection.assignments.len(),
        "WorkGraph allocator projection contains duplicate assignment source keys"
    );
    let mut assignment_ids = BTreeSet::new();
    for assignment in &projection.assignments {
        let document = artifacts
            .get(&assignment.source_key)
            .context("WorkGraph assignment has no authenticated artifact")?;
        let task = task_bindings
            .get(assignment.task_source_key.as_str())
            .context("WorkGraph assignment has no accepted task binding")?;
        anyhow::ensure!(
            document.task_source_key == assignment.task_source_key
                && document.body.starts_with(WORKGRAPH_ASSIGN_MARKER)
                && task.task_id == assignment.task_id
                && valid_workgraph_id(&assignment.assignment_id)
                && assignment_ids.insert(&assignment.assignment_id)
                && !assignment.permitted_executors.is_empty()
                && assignment.permitted_executors.len() <= MAX_WORKGRAPH_PERMITTED_EXECUTORS
                && assignment
                    .permitted_executors
                    .iter()
                    .all(|executor| valid_workgraph_id(executor))
                && assignment
                    .permitted_executors
                    .iter()
                    .collect::<BTreeSet<_>>()
                    .len()
                    == assignment.permitted_executors.len(),
            "WorkGraph allocator projection contains an invalid or duplicate assignment"
        );
    }

    let mut dispatch_sources = BTreeSet::new();
    let mut dispatch_leases = BTreeSet::new();
    for dispatch in &projection.dispatches {
        let document = artifacts
            .get(&dispatch.source_key)
            .context("WorkGraph dispatch has no authenticated artifact")?;
        let assignment = projection
            .assignments
            .iter()
            .find(|assignment| assignment.assignment_id == dispatch.assignment_id)
            .context("WorkGraph dispatch has no accepted assignment")?;
        anyhow::ensure!(
            dispatch_sources.insert(&dispatch.source_key)
                && dispatch_leases.insert(&dispatch.lease_id)
                && document.task_source_key == dispatch.task_source_key
                && document.body.starts_with(WORKGRAPH_DISPATCH_MARKER)
                && assignment.task_source_key == dispatch.task_source_key
                && assignment.task_id == dispatch.task_id
                && assignment
                    .permitted_executors
                    .contains(&dispatch.executor_id)
                && [
                    &dispatch.task_id,
                    &dispatch.assignment_id,
                    &dispatch.lease_id,
                    &dispatch.executor_id,
                    &dispatch.slot_id,
                ]
                .into_iter()
                .all(|value| valid_workgraph_id(value)),
            "WorkGraph allocator projection contains an invalid or duplicate dispatch"
        );
    }
    Ok(())
}

fn valid_workgraph_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_WORKGRAPH_ID_LENGTH
}

fn workgraph_assignment_matches(
    state: &WorkGraphAssignmentState,
    desired: &WorkGraphAssignmentBinding,
) -> bool {
    state.task_source_key == desired.task_source_key
        && state.task_id == desired.task_id
        && state.assignment_id == desired.assignment_id
        && state.permitted_executors == desired.permitted_executors
}

fn workgraph_dispatch_matches(
    lease: &WorkGraphActiveLease,
    desired: &WorkGraphDispatchBinding,
) -> bool {
    lease.has_dispatch
        && lease.task_source_key == desired.task_source_key
        && lease.task_id == desired.task_id
        && lease.assignment_id == desired.assignment_id
        && lease.lease_id == desired.lease_id
        && lease.executor_id == desired.executor_id
        && lease.slot_id == desired.slot_id
}

impl Default for AllocationState {
    fn default() -> Self {
        Self {
            version: VERSION,
            agents: BTreeMap::new(),
            workgraph_task_identities: BTreeMap::new(),
            workgraph_assignments: BTreeMap::new(),
            workgraph_assignment_attempts: BTreeMap::new(),
            workgraph_active: BTreeMap::new(),
            workgraph_dispatched: BTreeMap::new(),
            workgraph_result_claims: BTreeMap::new(),
            pending: Vec::new(),
            pending_offset: 0,
            workgraph_checkpoint: Vec::new(),
            workgraph_tasks: BTreeMap::new(),
            workgraph_root_issues: BTreeMap::new(),
            workgraph_issue_revisions: BTreeMap::new(),
            workgraph_artifacts: BTreeMap::new(),
            pending_workgraph_origins: BTreeSet::new(),
        }
    }
}

impl AllocationState {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!(
                "allocator state version must equal {VERSION}; clear source state before starting \
                 this prototype revision"
            ));
        }
        if self.pending_offset > self.pending.len()
            || (self.pending.is_empty() && self.pending_offset != 0)
        {
            return Err("allocator pending WAL offset is invalid".to_string());
        }
        let mut slots = BTreeSet::new();
        let mut canonical_task_ids = BTreeSet::new();
        let mut canonical_task_elements = BTreeSet::new();
        for (source_key, task) in &self.workgraph_task_identities {
            if source_key.trim().is_empty()
                || task.task_id.trim().is_empty()
                || task.task_element_id.trim().is_empty()
                || !canonical_task_ids.insert(&task.task_id)
                || !canonical_task_elements.insert(&task.task_element_id)
            {
                return Err("WorkGraph task identity state is invalid or duplicated".into());
            }
        }
        let mut canonical_assignment_ids = BTreeSet::new();
        for (source_key, assignment) in &self.workgraph_assignments {
            let task = self
                .workgraph_task_identities
                .get(&assignment.task_source_key)
                .ok_or("WorkGraph assignment has no projected task")?;
            let executors = assignment
                .permitted_executors
                .iter()
                .collect::<BTreeSet<_>>();
            if source_key.trim().is_empty()
                || assignment.assignment_id.trim().is_empty()
                || assignment.task_id != task.task_id
                || (!task.is_open && assignment.eligible)
                || assignment.permitted_executors.is_empty()
                || executors.len() != assignment.permitted_executors.len()
                || assignment
                    .permitted_executors
                    .iter()
                    .any(|executor| executor.trim().is_empty())
                || !canonical_assignment_ids.insert(&assignment.assignment_id)
                || self
                    .workgraph_assignment_attempts
                    .get(&assignment.assignment_id)
                    .copied()
                    .unwrap_or_default()
                    != assignment.next_attempt
            {
                return Err("WorkGraph assignment state violates canonical invariants".into());
            }
        }
        let mut active_workgraph_tasks = BTreeSet::new();
        for (lease_id, lease) in &self.workgraph_active {
            let assignment = self
                .workgraph_assignments
                .get(&lease.assignment_source_key)
                .ok_or("WorkGraph active lease has no assignment")?;
            let task = self
                .workgraph_task_identities
                .get(&lease.task_source_key)
                .ok_or("WorkGraph active lease has no task")?;
            let agent = self
                .agents
                .get(&lease.executor_id)
                .ok_or("WorkGraph active lease has no executor")?;
            if lease_id != &lease.lease_id
                || lease.task_id != task.task_id
                || lease.task_element_id != task.task_element_id
                || assignment.task_source_key != lease.task_source_key
                || assignment.task_id != lease.task_id
                || assignment.assignment_id != lease.assignment_id
                || !assignment.permitted_executors.contains(&lease.executor_id)
                || assignment.eligible
                || assignment.next_attempt == 0
                || lease.completed
                || !lease.completion_eligible
                || lease_id
                    != &make_lease_id(
                        &lease.task_id,
                        &lease.assignment_id,
                        assignment.next_attempt,
                    )
                || lease.slot_id != slot_id(&lease.executor_id, lease.slot_number)
                || !task.is_open
                || (lease.slot_number > agent.configured_slots
                    && !agent.retiring_slots.contains(&lease.slot_number))
                || !active_workgraph_tasks.insert(&lease.task_id)
                || !slots.insert(&lease.slot_id)
            {
                return Err("WorkGraph active lease violates canonical invariants".into());
            }
            let acquired =
                DateTime::parse_from_rfc3339(&lease.acquired_at).map_err(|e| e.to_string())?;
            let expires =
                DateTime::parse_from_rfc3339(&lease.expires_at).map_err(|e| e.to_string())?;
            if acquired >= expires {
                return Err("WorkGraph active lease acquiredAt must precede expiresAt".into());
            }
        }
        let mut dispatched_lease_ids = BTreeSet::new();
        let mut completed_workgraph_tasks = BTreeSet::new();
        for (dispatch_source, lease) in &self.workgraph_dispatched {
            let assignment = self
                .workgraph_assignments
                .get(&lease.assignment_source_key)
                .ok_or("WorkGraph dispatched lease has no assignment")?;
            let task = self
                .workgraph_task_identities
                .get(&lease.task_source_key)
                .ok_or("WorkGraph dispatched lease has no task")?;
            if dispatch_source.trim().is_empty()
                || !dispatched_lease_ids.insert(&lease.lease_id)
                || lease.task_id != task.task_id
                || lease.task_element_id != task.task_element_id
                || assignment.task_source_key != lease.task_source_key
                || assignment.task_id != lease.task_id
                || assignment.assignment_id != lease.assignment_id
                || !assignment.permitted_executors.contains(&lease.executor_id)
                || !lease.has_dispatch
                || (lease.completed && !lease.completion_eligible)
                || lease.slot_number == 0
                || lease.slot_id != slot_id(&lease.executor_id, lease.slot_number)
                || [
                    &lease.lease_id,
                    &lease.task_id,
                    &lease.assignment_id,
                    &lease.executor_id,
                    &lease.slot_id,
                ]
                .into_iter()
                .any(|value| !valid_workgraph_id(value))
                || self
                    .workgraph_active
                    .get(&lease.lease_id)
                    .is_some_and(|active| active != lease)
                || (lease.completed
                    && (!completed_workgraph_tasks.insert(&lease.task_source_key) || task.is_open))
            {
                return Err("WorkGraph dispatched lease violates canonical invariants".into());
            }
            let acquired =
                DateTime::parse_from_rfc3339(&lease.acquired_at).map_err(|e| e.to_string())?;
            let expires =
                DateTime::parse_from_rfc3339(&lease.expires_at).map_err(|e| e.to_string())?;
            if acquired >= expires {
                return Err("WorkGraph dispatched lease acquiredAt must precede expiresAt".into());
            }
        }
        if self.workgraph_active.values().any(|lease| {
            lease.has_dispatch
                != self
                    .workgraph_dispatched
                    .values()
                    .any(|dispatched| dispatched.lease_id == lease.lease_id)
        }) {
            return Err(
                "WorkGraph active lease Dispatch state does not match accepted artifacts".into(),
            );
        }
        if self
            .workgraph_active
            .values()
            .any(|lease| completed_workgraph_tasks.contains(&lease.task_source_key))
        {
            return Err("WorkGraph task has multiple selected leases".into());
        }
        if self
            .workgraph_result_claims
            .iter()
            .any(|(lease_id, claim_id)| {
                claim_id.trim().is_empty()
                    || !self
                        .workgraph_active
                        .get(lease_id)
                        .is_some_and(|lease| lease.has_dispatch)
            })
        {
            return Err("WorkGraph Result claim does not identify an active Dispatch lease".into());
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

    fn reconcile_workgraph(
        &mut self,
        projection: WorkGraphAllocatorProjection,
        task_documents: &BTreeMap<String, TaskDocument>,
        effective_from: u64,
        now: DateTime<Utc>,
    ) -> AnyResult<AllocationDelta> {
        let mut delta = AllocationDelta::default();
        let desired_tasks = projection
            .tasks
            .iter()
            .map(|task| (task.source_key.as_str(), task))
            .collect::<BTreeMap<_, _>>();
        for source_key in self
            .workgraph_task_identities
            .keys()
            .filter(|source_key| !desired_tasks.contains_key(source_key.as_str()))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.cancel_workgraph_task(&source_key, true, &mut delta);
        }
        for binding in &projection.tasks {
            let document = task_documents
                .get(&binding.source_key)
                .expect("projection provenance was validated");
            let identity_changed = self
                .workgraph_task_identities
                .get(&binding.source_key)
                .is_some_and(|task| {
                    task.task_id != binding.task_id
                        || task.task_element_id != binding.task_element_id
                });
            if identity_changed {
                self.cancel_workgraph_task(&binding.source_key, true, &mut delta);
            }
            let reopening = self
                .workgraph_task_identities
                .get(&binding.source_key)
                .is_some_and(|task| !task.is_open && document.is_open);
            if reopening {
                self.reopen_workgraph_task(&binding.source_key, &mut delta);
            }
            self.workgraph_task_identities.insert(
                binding.source_key.clone(),
                WorkGraphTaskState {
                    task_id: binding.task_id.clone(),
                    task_element_id: binding.task_element_id.clone(),
                    is_open: document.is_open,
                },
            );
            if !document.is_open {
                self.deactivate_workgraph_task(&binding.source_key, &mut delta);
            }
        }

        let desired_assignments = projection
            .assignments
            .iter()
            .map(|assignment| (assignment.source_key.as_str(), assignment))
            .collect::<BTreeMap<_, _>>();
        for source_key in self
            .workgraph_assignments
            .iter()
            .filter(|(source_key, state)| {
                desired_assignments
                    .get(source_key.as_str())
                    .is_none_or(|desired| !workgraph_assignment_matches(state, desired))
            })
            .map(|(source_key, _)| source_key.clone())
            .collect::<Vec<_>>()
        {
            self.retract_workgraph_assignment(&source_key, &mut delta);
        }
        for assignment in &projection.assignments {
            if let Some(state) = self.workgraph_assignments.get_mut(&assignment.source_key) {
                let task_open = self
                    .workgraph_task_identities
                    .get(&state.task_source_key)
                    .is_some_and(|task| task.is_open);
                let owned = self
                    .workgraph_active
                    .values()
                    .any(|lease| lease.assignment_source_key == assignment.source_key);
                let eligible = task_open && !owned;
                if state.eligible != eligible {
                    state.eligible = eligible;
                    delta
                        .affected_agents
                        .extend(state.permitted_executors.iter().cloned());
                }
                continue;
            }
            let task = self
                .workgraph_task_identities
                .get(&assignment.task_source_key)
                .context("WorkGraph assignment references an unknown task")?;
            let next_attempt = self
                .workgraph_assignment_attempts
                .get(&assignment.assignment_id)
                .copied()
                .unwrap_or_default();
            self.workgraph_assignments.insert(
                assignment.source_key.clone(),
                WorkGraphAssignmentState {
                    task_source_key: assignment.task_source_key.clone(),
                    task_id: assignment.task_id.clone(),
                    assignment_id: assignment.assignment_id.clone(),
                    permitted_executors: assignment.permitted_executors.clone(),
                    queued_at: effective_from,
                    next_attempt,
                    eligible: task.is_open,
                },
            );
            delta
                .affected_agents
                .extend(assignment.permitted_executors.iter().cloned());
        }

        let desired_dispatches = projection
            .dispatches
            .iter()
            .map(|dispatch| (dispatch.source_key.as_str(), dispatch))
            .collect::<BTreeMap<_, _>>();
        for source_key in self
            .workgraph_dispatched
            .iter()
            .filter(|(source_key, lease)| {
                desired_dispatches
                    .get(source_key.as_str())
                    .is_none_or(|desired| !workgraph_dispatch_matches(lease, desired))
            })
            .map(|(source_key, _)| source_key.clone())
            .collect::<Vec<_>>()
        {
            self.retract_workgraph_dispatch(&source_key, &mut delta);
        }

        self.allocate_workgraph(now, &mut delta);
        let now_text = timestamp(now);

        for dispatch in &projection.dispatches {
            if self.workgraph_dispatched.contains_key(&dispatch.source_key) {
                continue;
            }
            let mut lease = self
                .workgraph_active
                .get(&dispatch.lease_id)
                .filter(|lease| {
                    lease.task_source_key == dispatch.task_source_key
                        && lease.task_id == dispatch.task_id
                        && lease.assignment_id == dispatch.assignment_id
                        && lease.executor_id == dispatch.executor_id
                        && lease.slot_id == dispatch.slot_id
                        && lease.expires_at > now_text
                })
                .cloned()
                .context("WorkGraph dispatch does not match the active Source lease")?;
            let assignment = self
                .workgraph_assignments
                .get(&lease.assignment_source_key)
                .context("WorkGraph dispatch lease has no queued assignment")?;
            anyhow::ensure!(
                assignment.assignment_id == dispatch.assignment_id
                    && assignment.task_id == dispatch.task_id
                    && assignment
                        .permitted_executors
                        .contains(&dispatch.executor_id),
                "WorkGraph dispatch violates its trusted assignment"
            );
            lease.has_dispatch = true;
            self.workgraph_active
                .insert(lease.lease_id.clone(), lease.clone());
            if let Some(started) = delta
                .workgraph_started
                .iter_mut()
                .find(|started| started.lease_id == lease.lease_id)
            {
                *started = lease.clone();
            } else {
                delta.workgraph_started.push(lease.clone());
            }
            self.workgraph_dispatched
                .insert(dispatch.source_key.clone(), lease);
        }

        self.allocate_workgraph(now, &mut delta);
        Ok(delta)
    }

    fn retract_workgraph_assignment(&mut self, source_key: &str, delta: &mut AllocationDelta) {
        let Some(assignment) = self.workgraph_assignments.remove(source_key) else {
            return;
        };
        for dispatch_source in self
            .workgraph_dispatched
            .iter()
            .filter(|(_, lease)| lease.assignment_source_key == source_key)
            .map(|(source, _)| source.clone())
            .collect::<Vec<_>>()
        {
            if let Some(lease) = self.workgraph_dispatched.remove(&dispatch_source) {
                delta.workgraph_historical_ended.push(lease);
            }
        }
        let leases = self
            .workgraph_active
            .values()
            .filter(|lease| lease.assignment_source_key == source_key)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in leases {
            self.release_workgraph(&lease_id, false, delta);
        }
        for executor in assignment.permitted_executors {
            delta.affected_agents.insert(executor);
        }
    }

    fn retract_workgraph_dispatch(&mut self, source_key: &str, delta: &mut AllocationDelta) {
        let Some(lease) = self.workgraph_dispatched.remove(source_key) else {
            return;
        };
        if let Some(active) = self.workgraph_active.get_mut(&lease.lease_id) {
            active.has_dispatch = false;
            self.workgraph_result_claims.remove(&lease.lease_id);
            let updated = active.clone();
            if let Some(started) = delta
                .workgraph_started
                .iter_mut()
                .find(|started| started.lease_id == updated.lease_id)
            {
                *started = updated;
            } else {
                delta.workgraph_started.push(updated);
            }
            return;
        }
        let assignment_active = self
            .workgraph_active
            .values()
            .any(|active| active.assignment_source_key == lease.assignment_source_key);
        if !assignment_active {
            if let Some(assignment) = self
                .workgraph_assignments
                .get_mut(&lease.assignment_source_key)
            {
                let eligible = self
                    .workgraph_task_identities
                    .get(&assignment.task_source_key)
                    .is_some_and(|task| task.is_open);
                if assignment.eligible != eligible {
                    assignment.eligible = eligible;
                    delta
                        .affected_agents
                        .extend(assignment.permitted_executors.iter().cloned());
                }
            }
        }
        delta.affected_agents.insert(lease.executor_id.clone());
        delta.workgraph_historical_ended.push(lease);
    }

    fn cancel_workgraph_task(
        &mut self,
        source_key: &str,
        remove_identity: bool,
        delta: &mut AllocationDelta,
    ) {
        for assignment_source in self
            .workgraph_assignments
            .iter()
            .filter(|(_, assignment)| assignment.task_source_key == source_key)
            .map(|(source, _)| source.clone())
            .collect::<Vec<_>>()
        {
            self.retract_workgraph_assignment(&assignment_source, delta);
        }
        for dispatch_source in self
            .workgraph_dispatched
            .iter()
            .filter(|(_, lease)| lease.task_source_key == source_key)
            .map(|(source, _)| source.clone())
            .collect::<Vec<_>>()
        {
            if let Some(lease) = self.workgraph_dispatched.remove(&dispatch_source) {
                delta.workgraph_historical_ended.push(lease);
            }
        }
        if remove_identity {
            self.workgraph_task_identities.remove(source_key);
        } else if let Some(task) = self.workgraph_task_identities.get_mut(source_key) {
            task.is_open = false;
        }
    }

    fn deactivate_workgraph_task(&mut self, source_key: &str, delta: &mut AllocationDelta) {
        let already_completed = self
            .workgraph_dispatched
            .values()
            .any(|lease| lease.task_source_key == source_key && lease.completed);
        let leases = self
            .workgraph_active
            .values()
            .filter(|lease| lease.task_source_key == source_key)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        let mut completed = false;
        for lease_id in leases {
            let terminal = self
                .workgraph_active
                .get(&lease_id)
                .is_some_and(|lease| lease.has_dispatch);
            self.release_workgraph(&lease_id, terminal, delta);
            completed |= terminal;
        }
        if !completed && !already_completed {
            let historical_source_key = self
                .workgraph_dispatched
                .iter()
                .filter(|(_, lease)| {
                    lease.task_source_key == source_key
                        && lease.completion_eligible
                        && !lease.completed
                })
                .max_by_key(|(_, lease)| (&lease.acquired_at, &lease.lease_id))
                .map(|(source_key, _)| source_key.clone());
            if let Some(source_key) = historical_source_key {
                let lease = self
                    .workgraph_dispatched
                    .get_mut(&source_key)
                    .expect("selected historical lease exists");
                lease.completed = true;
                delta.workgraph_historical.push(lease.clone());
            }
        }

        for assignment in self
            .workgraph_assignments
            .values_mut()
            .filter(|assignment| assignment.task_source_key == source_key)
        {
            if assignment.eligible {
                assignment.eligible = false;
                delta
                    .affected_agents
                    .extend(assignment.permitted_executors.iter().cloned());
            }
        }
    }

    fn reopen_workgraph_task(&mut self, source_key: &str, delta: &mut AllocationDelta) {
        for lease in self.workgraph_dispatched.values_mut().filter(|lease| {
            lease.task_source_key == source_key && (lease.completion_eligible || lease.completed)
        }) {
            lease.completed = false;
            lease.completion_eligible = false;
            delta.workgraph_historical.push(lease.clone());
        }
    }

    fn allocate_workgraph(&mut self, now: DateTime<Utc>, delta: &mut AllocationDelta) {
        let mut slots = self
            .agents
            .iter()
            .filter(|(_, agent)| agent.configured)
            .flat_map(|(executor_id, agent)| {
                (1..=agent.configured_slots)
                    .map(move |number| (number, slot_id(executor_id, number), executor_id.clone()))
            })
            .collect::<Vec<_>>();
        slots.sort();
        for (slot_number, slot, executor_id) in slots {
            if self
                .workgraph_active
                .values()
                .any(|lease| lease.slot_id == slot)
            {
                continue;
            }
            let mut queue = self
                .workgraph_assignments
                .iter()
                .filter(|(_, assignment)| {
                    assignment.eligible
                        && assignment.permitted_executors.contains(&executor_id)
                        && !self
                            .workgraph_active
                            .values()
                            .any(|lease| lease.task_id == assignment.task_id)
                        && self
                            .workgraph_task_identities
                            .get(&assignment.task_source_key)
                            .is_some_and(|task| task.is_open && task.task_id == assignment.task_id)
                })
                .map(|(source_key, assignment)| {
                    (
                        assignment.queued_at,
                        assignment.task_id.clone(),
                        assignment.assignment_id.clone(),
                        source_key.clone(),
                    )
                })
                .collect::<Vec<_>>();
            queue.sort();
            let Some((_, _, assignment_id, assignment_source_key)) = queue.into_iter().next()
            else {
                continue;
            };
            let assignment = self
                .workgraph_assignments
                .get_mut(&assignment_source_key)
                .expect("queued WorkGraph assignment exists");
            let affected_executors = assignment.permitted_executors.clone();
            let attempt = self
                .workgraph_assignment_attempts
                .entry(assignment_id.clone())
                .or_insert(assignment.next_attempt);
            *attempt += 1;
            assignment.next_attempt = *attempt;
            assignment.eligible = false;
            let lease_id = make_lease_id(
                &assignment.task_id,
                &assignment.assignment_id,
                assignment.next_attempt,
            );
            let task = self
                .workgraph_task_identities
                .get(&assignment.task_source_key)
                .expect("validated WorkGraph assignment task exists");
            let agent = &self.agents[&executor_id];
            let lease = WorkGraphActiveLease {
                lease_id: lease_id.clone(),
                task_source_key: assignment.task_source_key.clone(),
                task_id: assignment.task_id.clone(),
                task_element_id: task.task_element_id.clone(),
                assignment_source_key,
                assignment_id,
                executor_id: executor_id.clone(),
                slot_id: slot,
                slot_number,
                acquired_at: timestamp(now),
                expires_at: timestamp(
                    now + chrono::Duration::seconds(agent.lease_duration_seconds),
                ),
                has_dispatch: false,
                completed: false,
                completion_eligible: true,
            };
            self.workgraph_active.insert(lease_id, lease.clone());
            delta.workgraph_started.push(lease);
            delta.affected_agents.extend(affected_executors);
        }
    }

    fn release_workgraph(&mut self, lease_id: &str, completed: bool, delta: &mut AllocationDelta) {
        let Some(mut lease) = self.workgraph_active.remove(lease_id) else {
            return;
        };
        self.workgraph_result_claims.remove(lease_id);
        lease.completed = completed;
        delta.affected_agents.insert(lease.executor_id.clone());
        if let Some(dispatched) = self
            .workgraph_dispatched
            .values_mut()
            .find(|dispatched| dispatched.lease_id == lease.lease_id)
        {
            *dispatched = lease.clone();
            delta.workgraph_released.push(lease.clone());
            delta.workgraph_historical.push(lease.clone());
        } else {
            delta.workgraph_ended.push(lease.clone());
        }
        if let Some(agent) = self
            .agents
            .get_mut(&lease.executor_id)
            .filter(|agent| agent.retiring_slots.contains(&lease.slot_number))
        {
            agent.retiring_slots.remove(&lease.slot_number);
            delta
                .removed_slots
                .insert((lease.executor_id.clone(), lease.slot_number));
            if !agent.configured && agent.retiring_slots.is_empty() {
                self.agents.remove(&lease.executor_id);
                delta.removed_agents.insert(lease.executor_id);
            }
        }
    }

    pub fn expire(&mut self, now: DateTime<Utc>) -> AllocationDelta {
        let now_text = timestamp(now);
        let mut delta = AllocationDelta::default();
        let mut workgraph_expired = self
            .workgraph_active
            .values()
            .filter(|lease| lease.expires_at.as_str() <= now_text.as_str())
            .map(|lease| (lease.expires_at.clone(), lease.lease_id.clone()))
            .collect::<Vec<_>>();
        workgraph_expired.sort();
        for (_, lease_id) in workgraph_expired {
            if let Some(assignment_source) = self
                .workgraph_active
                .get(&lease_id)
                .map(|lease| lease.assignment_source_key.clone())
            {
                if let Some(assignment) = self.workgraph_assignments.get_mut(&assignment_source) {
                    assignment.eligible = true;
                    delta
                        .affected_agents
                        .extend(assignment.permitted_executors.iter().cloned());
                }
            }
            self.release_workgraph(&lease_id, false, &mut delta);
        }
        self.allocate_workgraph(now, &mut delta);
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
        self.allocate_workgraph(now, &mut delta);
        delta
    }

    pub fn workgraph_active_exact(
        &self,
        task_id: &str,
        lease_id: &str,
        assignment_id: &str,
        executor_id: &str,
        slot_id: &str,
        now: DateTime<Utc>,
    ) -> Option<&WorkGraphActiveLease> {
        let now = timestamp(now);
        self.workgraph_active.get(lease_id).filter(|active| {
            active.task_id == task_id
                && active.assignment_id == assignment_id
                && active.executor_id == executor_id
                && active.slot_id == slot_id
                && active.expires_at > now
        })
    }

    pub fn agent_runtime(&self) -> BTreeMap<String, AgentRuntime> {
        self.agents
            .iter()
            .map(|(id, agent)| {
                let occupied = self.active_slots(id);
                let queue_depth = self
                    .workgraph_assignments
                    .values()
                    .filter(|assignment| {
                        assignment.eligible
                            && assignment.permitted_executors.contains(id)
                            && !self
                                .workgraph_active
                                .values()
                                .any(|lease| lease.task_id == assignment.task_id)
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
            workgraph_started: self.workgraph_active.values().cloned().collect(),
            workgraph_historical: self
                .workgraph_dispatched
                .values()
                .filter(|lease| !self.workgraph_active.contains_key(&lease.lease_id))
                .cloned()
                .collect(),
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

    fn active_slots(&self, agent: &str) -> BTreeSet<u32> {
        self.workgraph_active
            .values()
            .filter(|lease| lease.executor_id == agent)
            .map(|lease| lease.slot_number)
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PreparedProjection, PreparedProjectionCommit, WorkGraphTaskBinding};
    use async_trait::async_trait;
    use chrono::TimeZone;
    use drasi_lib::wal::WriteAheadLogConfig;
    use drasi_lib::MemoryStateStoreProvider;
    use drasi_wal_redb::RedbWalProvider;
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    struct RecordingDispatchProjector {
        committed: Arc<TokioMutex<Vec<Vec<ProjectionInput>>>>,
        replacement: WorkGraphDispatchBinding,
    }

    struct RecordingDispatchCommit {
        inputs: Vec<ProjectionInput>,
        committed: Arc<TokioMutex<Vec<Vec<ProjectionInput>>>>,
    }

    #[async_trait]
    impl PreparedProjectionCommit for RecordingDispatchCommit {
        async fn commit(self: Box<Self>) {
            self.committed.lock().await.push(self.inputs);
        }
    }

    #[async_trait]
    impl WorkGraphProjector for RecordingDispatchProjector {
        async fn prepare(
            &self,
            inputs: Vec<ProjectionInput>,
            _effective_from: u64,
        ) -> AnyResult<PreparedProjection> {
            let retracting = matches!(
                inputs.as_slice(),
                [ProjectionInput::DeleteLifecycleArtifact { .. }]
            );
            Ok(PreparedProjection {
                changes: Vec::new(),
                allocator: projection(if retracting {
                    Vec::new()
                } else {
                    vec![self.replacement.clone()]
                }),
                rejection: None,
                state_changed: true,
                checkpoint: vec![u8::from(retracting) + 1],
                commit: Box::new(RecordingDispatchCommit {
                    inputs,
                    committed: self.committed.clone(),
                }),
            })
        }

        async fn restore(&self, _checkpoint: &[u8]) -> AnyResult<()> {
            Ok(())
        }

        fn source_id(&self) -> &str {
            "source"
        }
    }

    fn projection(dispatches: Vec<WorkGraphDispatchBinding>) -> WorkGraphAllocatorProjection {
        WorkGraphAllocatorProjection {
            tasks: vec![WorkGraphTaskBinding {
                source_key: "issue".to_string(),
                task_id: "task".to_string(),
                task_element_id: "task-element".to_string(),
            }],
            assignments: vec![WorkGraphAssignmentBinding {
                source_key: "assignment-comment".to_string(),
                task_source_key: "issue".to_string(),
                task_id: "task".to_string(),
                assignment_id: "assignment".to_string(),
                permitted_executors: vec!["executor".to_string()],
            }],
            dispatches,
        }
    }

    #[tokio::test]
    async fn invalid_dispatch_replacement_durably_retracts_prior_authorization() {
        let now = Utc::now();
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile {
                version: 1,
                agents: vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            },
            now,
        );
        let documents = BTreeMap::from([(
            "issue".to_string(),
            TaskDocument {
                source_key: "issue".to_string(),
                body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                is_open: true,
                state_reason: String::new(),
                parent_source_key: None,
            },
        )]);
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate lease");
        let lease = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("active lease");
        let accepted_dispatch = WorkGraphDispatchBinding {
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            task_id: "task".to_string(),
            assignment_id: "assignment".to_string(),
            lease_id: lease.lease_id.clone(),
            executor_id: "executor".to_string(),
            slot_id: lease.slot_id.clone(),
        };
        state
            .reconcile_workgraph(
                projection(vec![accepted_dispatch.clone()]),
                &documents,
                2,
                now,
            )
            .expect("accept initial Dispatch");
        state.workgraph_tasks = documents;
        state.workgraph_artifacts = BTreeMap::from([
            (
                "assignment-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "assignment-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_ASSIGN_MARKER}\n"),
                },
            ),
            (
                "dispatch-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "dispatch-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_DISPATCH_MARKER}\n"),
                },
            ),
        ]);
        state.workgraph_checkpoint = vec![1];
        state.validate().expect("seed state is valid");

        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        store
            .set(
                "source",
                STATE_KEY,
                serde_json::to_vec(&state).expect("serialize state"),
            )
            .await
            .expect("seed state");
        let wal = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        wal.register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");
        let allocator = Allocator::new("source".to_string(), store, wal);
        let committed = Arc::new(TokioMutex::new(Vec::new()));
        let projector = RecordingDispatchProjector {
            committed: committed.clone(),
            replacement: WorkGraphDispatchBinding {
                lease_id: "not-the-active-lease".to_string(),
                ..accepted_dispatch
            },
        };
        let replacement = LifecycleArtifactDocument {
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            body: format!("{WORKGRAPH_DISPATCH_MARKER}\nreplacement"),
        };

        let (_, rejection) = allocator
            .ingest_workgraph(
                &projector,
                vec![ProjectionInput::UpsertLifecycleArtifact(
                    replacement.clone(),
                )],
                3,
                "invalid-replacement",
            )
            .await
            .expect("persist fail-closed retraction");
        assert!(rejection
            .as_deref()
            .is_some_and(|message| message.contains("active Source lease")));
        assert!(allocator
            .claim_active(
                "task",
                &lease.lease_id,
                "assignment",
                "executor",
                &lease.slot_id,
                "claim",
                Utc::now(),
            )
            .await
            .expect("read authorization")
            .is_none());
        let (_, repeated_rejection) = allocator
            .ingest_workgraph(
                &projector,
                vec![ProjectionInput::UpsertLifecycleArtifact(replacement)],
                4,
                "repeated-invalid-replacement",
            )
            .await
            .expect("deduplicate repeated fail-closed retraction");
        assert!(repeated_rejection.is_some());
        assert_eq!(
            allocator
                .workgraph_checkpoint()
                .await
                .expect("read checkpoint"),
            vec![2]
        );
        let commits = committed.lock().await;
        assert_eq!(commits.len(), 2);
        assert!(commits.iter().all(|inputs| matches!(
            inputs.as_slice(),
            [ProjectionInput::DeleteLifecycleArtifact { source_key }]
                if source_key == "dispatch-comment"
        )));
    }

    #[test]
    fn dispatched_lease_remains_active_until_expiry_then_retries() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile {
                version: 1,
                agents: vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            },
            now,
        );
        let documents = BTreeMap::from([(
            "issue".to_string(),
            TaskDocument {
                source_key: "issue".to_string(),
                body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                is_open: true,
                state_reason: String::new(),
                parent_source_key: None,
            },
        )]);

        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate first lease");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("first lease");
        let dispatch = WorkGraphDispatchBinding {
            source_key: "dispatch-comment-1".to_string(),
            task_source_key: "issue".to_string(),
            task_id: "task".to_string(),
            assignment_id: "assignment".to_string(),
            lease_id: first.lease_id.clone(),
            executor_id: "executor".to_string(),
            slot_id: first.slot_id.clone(),
        };
        let mut expired = state.clone();
        assert!(expired
            .reconcile_workgraph(
                projection(vec![dispatch.clone()]),
                &documents,
                2,
                now + chrono::Duration::seconds(61),
            )
            .is_err());
        state
            .reconcile_workgraph(
                projection(vec![dispatch.clone()]),
                &documents,
                2,
                now + chrono::Duration::seconds(1),
            )
            .expect("accept dispatch");
        assert!(state
            .workgraph_active_exact(
                "task",
                &first.lease_id,
                "assignment",
                "executor",
                &first.slot_id,
                now + chrono::Duration::seconds(1),
            )
            .is_some());
        let dispatched = state
            .workgraph_active
            .get(&first.lease_id)
            .cloned()
            .expect("dispatched lease remains active");
        assert!(dispatched.has_dispatch);
        assert_eq!(
            state.workgraph_dispatched.get("dispatch-comment-1"),
            Some(&dispatched)
        );

        let expiry = state.expire(now + chrono::Duration::seconds(61));
        assert_eq!(expiry.workgraph_released, vec![dispatched.clone()]);
        assert_eq!(expiry.workgraph_historical, vec![dispatched]);
        assert_eq!(expiry.workgraph_started.len(), 1);
        let second = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("retry lease");
        assert_ne!(first.lease_id, second.lease_id);
        assert!(!second.has_dispatch);
        assert!(!second.completed);
        assert!(state
            .workgraph_active_exact(
                "task",
                &first.lease_id,
                "assignment",
                "executor",
                &first.slot_id,
                now + chrono::Duration::seconds(61),
            )
            .is_none());
        assert!(state
            .workgraph_active_exact(
                "task",
                &second.lease_id,
                "assignment",
                "executor",
                &second.slot_id,
                now + chrono::Duration::seconds(61),
            )
            .is_some());
        state.validate().expect("valid retry state");
        state
            .reconcile_workgraph(
                projection(vec![dispatch.clone()]),
                &documents,
                3,
                now + chrono::Duration::seconds(61),
            )
            .expect("retain stale authenticated dispatch during retry");

        let mut closed_documents = documents.clone();
        closed_documents
            .get_mut("issue")
            .expect("task document")
            .is_open = false;
        let closed = state
            .reconcile_workgraph(
                projection(vec![dispatch.clone()]),
                &closed_documents,
                4,
                now + chrono::Duration::seconds(62),
            )
            .expect("close task after terminal lease expiry");
        assert!(state.workgraph_active.is_empty());
        assert!(state
            .workgraph_dispatched
            .get("dispatch-comment-1")
            .is_some_and(|lease| lease.completed));
        assert!(closed
            .workgraph_historical
            .iter()
            .any(|lease| lease.lease_id == first.lease_id && lease.completed));
        assert!(!state
            .workgraph_dispatched
            .values()
            .any(|lease| lease.lease_id == second.lease_id));

        let mut other_history = second.clone();
        other_history.has_dispatch = true;
        state
            .workgraph_dispatched
            .insert("dispatch-comment-2".to_string(), other_history);
        let mut repeated = AllocationDelta::default();
        state.deactivate_workgraph_task("issue", &mut repeated);
        assert!(
            !state
                .workgraph_dispatched
                .get("dispatch-comment-2")
                .expect("other historical lease")
                .completed
        );
        assert!(repeated.workgraph_historical.is_empty());

        let reopened = state
            .reconcile_workgraph(
                projection(vec![dispatch]),
                &documents,
                5,
                now + chrono::Duration::seconds(63),
            )
            .expect("reopen completed task");
        assert_eq!(state.workgraph_active.len(), 1);
        assert!(!state
            .workgraph_dispatched
            .values()
            .any(|lease| lease.completed));
        assert_eq!(
            state.workgraph_active.len()
                + state
                    .workgraph_dispatched
                    .values()
                    .filter(|lease| lease.completed)
                    .count(),
            1
        );
        assert!(reopened
            .workgraph_historical
            .iter()
            .any(|lease| lease.lease_id == first.lease_id && !lease.completed));

        let reclosed = state
            .reconcile_workgraph(
                projection(vec![WorkGraphDispatchBinding {
                    source_key: "dispatch-comment-1".to_string(),
                    task_source_key: "issue".to_string(),
                    task_id: "task".to_string(),
                    assignment_id: "assignment".to_string(),
                    lease_id: first.lease_id.clone(),
                    executor_id: "executor".to_string(),
                    slot_id: first.slot_id.clone(),
                }]),
                &closed_documents,
                6,
                now + chrono::Duration::seconds(64),
            )
            .expect("close reopened task before its new lease is dispatched");
        assert!(state.workgraph_active.is_empty());
        assert!(!state
            .workgraph_dispatched
            .values()
            .any(|lease| lease.completed));
        assert!(reclosed
            .workgraph_historical
            .iter()
            .all(|lease| !lease.completed));
    }

    #[test]
    fn queued_assignment_marks_affected_executor_without_a_free_slot() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile {
                version: 1,
                agents: vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            },
            now,
        );
        let documents = BTreeMap::from([
            (
                "issue".to_string(),
                TaskDocument {
                    source_key: "issue".to_string(),
                    body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: None,
                },
            ),
            (
                "issue-2".to_string(),
                TaskDocument {
                    source_key: "issue-2".to_string(),
                    body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: None,
                },
            ),
        ]);
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("occupy only slot");

        let mut queued = projection(Vec::new());
        queued.tasks.push(WorkGraphTaskBinding {
            source_key: "issue-2".to_string(),
            task_id: "task-2".to_string(),
            task_element_id: "task-element-2".to_string(),
        });
        queued.assignments.push(WorkGraphAssignmentBinding {
            source_key: "assignment-comment-2".to_string(),
            task_source_key: "issue-2".to_string(),
            task_id: "task-2".to_string(),
            assignment_id: "assignment-2".to_string(),
            permitted_executors: vec!["executor".to_string()],
        });
        let delta = state
            .reconcile_workgraph(queued, &documents, 2, now + chrono::Duration::seconds(1))
            .expect("queue second assignment");
        assert!(delta.affected_agents.contains("executor"));
        assert_eq!(state.agent_runtime()["executor"].queue_depth, 1);
    }
}
