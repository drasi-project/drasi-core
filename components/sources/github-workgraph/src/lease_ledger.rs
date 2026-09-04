// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::agents::{
    ActorDefinition, ActorGitHubIdentity, ActorKind, AgentDefinition, AgentFile, AgentFileContent,
    AgentFileLocation, MAX_AGENT_SLOTS,
};
use crate::mapping::{agent_changes, allocation_changes, generic_issue_changes, AgentProjection};
use crate::model::slot_id;
use crate::protocol::{
    derive_workgraph_id, derive_workgraph_response_body_digest, is_typed_workgraph_id,
    LifecycleArtifactDocument, PreparedProjectionCommit, ProjectionInput, RootIssueCommentDocument,
    RootIssueDocument, TaskDocument, TaskResponseDocument, TaskResponseRole,
    WorkGraphAllocatorProjection, WorkGraphAssignmentBinding, WorkGraphAssignmentRequestBinding,
    WorkGraphDispatchBinding, WorkGraphProjector, WorkGraphRouteBinding,
    MAX_ROOT_ISSUE_COMMENT_BODY_BYTES, MAX_TASK_RESPONSE_BODY_BYTES, MAX_WORKGRAPH_ATTEMPTS,
    WORKGRAPH_ASSIGNMENT_MARKER, WORKGRAPH_ASSIGNMENT_REQUEST_MARKER, WORKGRAPH_DISPATCH_MARKER,
    WORKGRAPH_EVALUATION_ACCEPTED, WORKGRAPH_EVALUATION_MARKER, WORKGRAPH_EVALUATION_REJECTED,
    WORKGRAPH_RESULT_MARKER, WORKGRAPH_ROUTE_MARKER, WORKGRAPH_ROUTE_REWORK,
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

const VERSION: u8 = 20;
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
    /// Catalog kind of the executor holding this lease. Operator metadata:
    /// leases are acquired at exactly the same lifecycle points for an agent
    /// and a human worker, and are never taken for an evaluation phase.
    #[serde(default)]
    pub actor_kind: ActorKind,
    /// The GitHub account of a human executor, when the catalog declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_github: Option<ActorGitHubIdentity>,
    pub task_source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    pub task_element_id: String,
    pub assignment_source_key: String,
    pub assignment_id: String,
    pub executor_id: String,
    pub slot_id: String,
    pub slot_number: u32,
    pub attempt: u64,
    pub acquired_at: String,
    pub expires_at: String,
    pub has_dispatch: bool,
    /// Canonical Dispatch ID once this lease has been dispatched. Empty until
    /// then, and empty when the projector supplies no Dispatch identity.
    #[serde(default)]
    pub dispatch_id: String,
    pub completed: bool,
    pub completion_eligible: bool,
    #[serde(default)]
    pub route_selected: bool,
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

#[derive(Clone, Debug)]
pub(crate) struct RootIssueCommentRevisionState {
    pub document: Option<RootIssueCommentDocument>,
    pub identity: RootIssueCommentIdentity,
    pub revision: i64,
    pub fingerprint: String,
    pub tombstone: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct LifecycleArtifactRevisionState {
    pub document: Option<LifecycleArtifactDocument>,
    pub revision: i64,
    pub tombstone: bool,
}

/// The open lifecycle subject a natural task response answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskResponseSubject {
    /// The actor holds the task's current active, dispatched lease.
    Worker {
        dispatch_id: String,
        lease_id: String,
    },
    /// The actor is the exact assigner an unanswered AssignmentRequest named.
    Assigner { request_id: String },
    /// No worker lease is held on the task and a Result is awaiting its
    /// Evaluation.
    Evaluator { result_id: String },
}

#[derive(Clone, Debug)]
pub(crate) struct TaskResponseRevisionState {
    pub document: Option<TaskResponseDocument>,
    pub identity: TaskResponseIdentity,
    pub revision: i64,
    pub fingerprint: String,
    pub tombstone: bool,
}

/// One Result a task is awaiting an Evaluation for, plus the executor whose
/// attempt produced it.
///
/// The producer is carried durably rather than looked up through the lease,
/// because a lease is transient: it expires, is released, is cancelled when
/// the task closes, and is gone entirely after a restart prunes it. Self
/// review must stay refused for as long as the Result is judgeable, so the
/// identity that must never evaluate it is recorded with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkGraphPendingResult {
    result_id: String,
    lease_id: String,
    /// The executor whose attempt produced this Result.
    executor_id: String,
    /// That executor's GitHub account, when it was a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    producer_github: Option<ActorGitHubIdentity>,
}

impl WorkGraphPendingResult {
    /// Whether one responder is the worker that produced this Result.
    ///
    /// Matched on the numeric GitHub ID, which survives a rename and both node
    /// ID encodings, and on the catalog actor ID, which covers a producer
    /// whose account was never recorded on the lease.
    fn produced_by(&self, author_database_id: u64, actor_id: &str) -> bool {
        self.executor_id == actor_id
            || self
                .producer_github
                .as_ref()
                .is_some_and(|github| github.database_id == author_database_id)
    }
}

/// One unanswered AssignmentRequest a task is currently holding open.
///
/// This is the assigner subject a natural response may answer. It carries the
/// named assigner and the candidate set verbatim so the response can be bound
/// to the exact question that was asked, and it is retired the moment a
/// decision-bound Assignment names its request.
///
/// A request allocates nothing. It has no lease, no slot, no attempt, and no
/// queue entry: the assigner is asked to decide, never to execute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkGraphPendingRequest {
    source_key: String,
    request_id: String,
    assigner_id: String,
    candidates: Vec<String>,
}

/// The identity a retracted task response keeps, so a delayed delivery is
/// fenced against the exact response it would otherwise resurrect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TaskResponseIdentity {
    pub source_key: String,
    pub task_source_key: String,
    pub task_id: String,
    pub actor_id: String,
}

impl From<&TaskResponseDocument> for TaskResponseIdentity {
    fn from(document: &TaskResponseDocument) -> Self {
        Self {
            source_key: document.source_key.clone(),
            task_source_key: document.task_source_key.clone(),
            task_id: document.task_id.clone(),
            actor_id: document.actor_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RootIssueCommentIdentity {
    pub source_key: String,
    pub root_issue_id: String,
    pub admission_id: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub repository_node_id: String,
    pub issue_number: u64,
}

impl From<&RootIssueCommentDocument> for RootIssueCommentIdentity {
    fn from(document: &RootIssueCommentDocument) -> Self {
        Self {
            source_key: document.source_key.clone(),
            root_issue_id: document.root_issue_id.clone(),
            admission_id: document.admission_id.clone(),
            repository_owner: document.repository_owner.clone(),
            repository_name: document.repository_name.clone(),
            repository_node_id: document.repository_node_id.clone(),
            issue_number: document.issue_number,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentState {
    agent_id: String,
    configured: bool,
    configured_slots: u32,
    lease_duration_seconds: i64,
    retiring_slots: BTreeSet<u32>,
    /// The catalog kind this executor was declared with. Defaulted so a state
    /// written before the actor catalog existed still loads as an agent.
    #[serde(default)]
    kind: ActorKind,
    /// The exact GitHub account a human executor speaks as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github: Option<ActorGitHubIdentity>,
    /// The custom agent an agent executor runs as. Empty for a legacy state.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    custom_agent: String,
}

impl AgentState {
    fn new(actor: &ActorDefinition) -> Self {
        Self {
            agent_id: actor.actor_id.clone(),
            configured: true,
            configured_slots: actor.slots,
            lease_duration_seconds: actor.lease_duration_seconds,
            retiring_slots: BTreeSet::new(),
            kind: actor.kind,
            github: actor.github.clone(),
            custom_agent: actor.custom_agent.clone(),
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
    root_issue_id: String,
    workflow_run_id: String,
    task_id: String,
    task_element_id: String,
    is_open: bool,
    workgraph_include: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkGraphAssignmentState {
    task_source_key: String,
    root_issue_id: String,
    workflow_run_id: String,
    task_id: String,
    assignment_id: String,
    permitted_executors: Vec<String>,
    queued_at: u64,
    next_attempt: u64,
    max_attempts: u64,
    eligible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkGraphAuthorizationState {
    root_issue_id: String,
    generation: u64,
    cutoff_revision: i64,
    transition_revision: i64,
    included: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkGraphRouteState {
    task_source_key: String,
    assignment_source_key: String,
    assignment_id: String,
    root_issue_id: String,
    workflow_run_id: String,
    task_id: String,
    result_id: String,
    evaluation_id: String,
    route_id: String,
    action: String,
    attempt: u64,
    max_attempts: u64,
    authorization_generation: u64,
    retry_executor_id: String,
    retry_slot_number: u32,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_routes: BTreeMap<String, WorkGraphRouteState>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_root_comments: BTreeMap<String, RootIssueCommentDocument>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_root_comment_revisions: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_root_comment_fingerprints: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_root_comment_tombstones: BTreeMap<String, RootIssueCommentIdentity>,
    /// Authenticated natural task responses, keyed by comment node ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_task_responses: BTreeMap<String, TaskResponseDocument>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_task_response_revisions: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_task_response_fingerprints: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_task_response_tombstones: BTreeMap<String, TaskResponseIdentity>,
    /// The Result each task Issue is currently awaiting an Evaluation for,
    /// with the executor that produced it. This is the open evaluator subject
    /// a natural response can answer, and the identity that may not answer it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_pending_results: BTreeMap<String, WorkGraphPendingResult>,
    /// The AssignmentRequest each task Issue is currently awaiting a decision
    /// for. This is the open assigner subject a natural response can answer.
    ///
    /// Defaulted and omitted when empty, so a checkpoint written before
    /// first-class assigners existed restores unchanged and one written
    /// without any pending request stays byte-identical to what it was.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_pending_requests: BTreeMap<String, WorkGraphPendingRequest>,
    /// Latest authoritative GitHub revision observed for a Root Issue or task Issue.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_issue_revisions: BTreeMap<String, i64>,
    /// Canonical state paired with each accepted Issue revision, including tombstones.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_issue_state_fingerprints: BTreeMap<String, String>,
    /// Numeric GitHub database issue IDs mapped to their GraphQL node IDs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_issue_database_ids: BTreeMap<u64, String>,
    /// Current authenticated lifecycle documents used to validate directives
    /// emitted later when out-of-order dependencies become available.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_artifacts: BTreeMap<String, LifecycleArtifactDocument>,
    /// Latest authoritative GitHub revision for each lifecycle comment, retained
    /// after deletion to fence delayed webhook deliveries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_artifact_revisions: BTreeMap<String, i64>,
    /// Durable task generations and timestamp cutoffs for actionable lifecycle evidence.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_authorizations: BTreeMap<String, WorkGraphAuthorizationState>,
    /// Generation assigned to each retained lifecycle artifact; zero is permanently stale.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workgraph_artifact_generations: BTreeMap<String, u64>,
    /// Assignment and Dispatch source keys observed while their task was
    /// excluded. They remain fenced across re-inclusion so an old artifact can
    /// never become fresh authorization again.
    workgraph_stale_authorizations: BTreeSet<String>,
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
            .prepare(projector_inputs(&accepted_inputs), effective_from)
            .await?;
        anyhow::ensure!(
            !prepared.checkpoint.is_empty(),
            "WorkGraph projector returned an empty recovery checkpoint"
        );
        let mut rejection = prepared.rejection.clone();
        let (
            mut next_root_issues,
            mut next_issue_revisions,
            mut next_issue_state_fingerprints,
            mut next_issue_database_ids,
            mut next_tasks,
            mut next_artifacts,
            mut next_artifact_revisions,
        ) = stage_workgraph_documents(&state, &accepted_inputs);
        let (
            mut next_root_comments,
            mut next_root_comment_revisions,
            mut next_root_comment_fingerprints,
            mut next_root_comment_tombstones,
        ) = stage_root_comment_documents(&state, &accepted_inputs)?;
        let (
            mut next_task_responses,
            mut next_task_response_revisions,
            mut next_task_response_fingerprints,
            mut next_task_response_tombstones,
        ) = stage_task_response_documents(&state, &accepted_inputs)?;
        let mut candidate = state.clone();
        candidate.refresh_workgraph_authorizations(
            &prepared.allocator,
            &next_tasks,
            &next_root_issues,
            &next_issue_revisions,
            &next_artifacts,
            &accepted_inputs,
        );
        candidate.fence_stale_workgraph_authorizations(&mut prepared.allocator);
        let transition =
            validate_workgraph_projection(&prepared.allocator, &next_tasks, &next_artifacts)
                .and_then(|()| {
                    candidate.reconcile_workgraph_with_roots(
                        prepared.allocator.clone(),
                        &next_tasks,
                        &next_root_issues,
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
                    .prepare(projector_inputs(&accepted_inputs), effective_from)
                    .await?;
                anyhow::ensure!(
                    !prepared.checkpoint.is_empty(),
                    "WorkGraph projector returned an empty recovery checkpoint"
                );
                append_rejection(&mut rejection, prepared.rejection.clone());
                (
                    next_root_issues,
                    next_issue_revisions,
                    next_issue_state_fingerprints,
                    next_issue_database_ids,
                    next_tasks,
                    next_artifacts,
                    next_artifact_revisions,
                ) = stage_workgraph_documents(&state, &accepted_inputs);
                (
                    next_root_comments,
                    next_root_comment_revisions,
                    next_root_comment_fingerprints,
                    next_root_comment_tombstones,
                ) = stage_root_comment_documents(&state, &accepted_inputs)?;
                (
                    next_task_responses,
                    next_task_response_revisions,
                    next_task_response_fingerprints,
                    next_task_response_tombstones,
                ) = stage_task_response_documents(&state, &accepted_inputs)?;
                candidate = state.clone();
                candidate.refresh_workgraph_authorizations(
                    &prepared.allocator,
                    &next_tasks,
                    &next_root_issues,
                    &next_issue_revisions,
                    &next_artifacts,
                    &accepted_inputs,
                );
                candidate.fence_stale_workgraph_authorizations(&mut prepared.allocator);
                validate_workgraph_projection(&prepared.allocator, &next_tasks, &next_artifacts)?;
                candidate.reconcile_workgraph_with_roots(
                    prepared.allocator.clone(),
                    &next_tasks,
                    &next_root_issues,
                    effective_from,
                    Utc::now(),
                )?
            }
        };
        state = candidate;
        let mut changes = generic_issue_changes(&self.source_id, effective_from, &accepted_inputs);
        changes.extend(prepared.changes);
        changes.extend(allocation_changes(
            &self.source_id,
            effective_from,
            &allocation_delta,
            &state.agent_runtime(),
        ));

        state.workgraph_tasks = next_tasks;
        state.workgraph_root_issues = next_root_issues;
        state.workgraph_root_comments = next_root_comments;
        state.workgraph_root_comment_revisions = next_root_comment_revisions;
        state.workgraph_root_comment_fingerprints = next_root_comment_fingerprints;
        state.workgraph_root_comment_tombstones = next_root_comment_tombstones;
        state.workgraph_task_responses = next_task_responses;
        state.workgraph_task_response_revisions = next_task_response_revisions;
        state.workgraph_task_response_fingerprints = next_task_response_fingerprints;
        state.workgraph_task_response_tombstones = next_task_response_tombstones;
        state.workgraph_issue_revisions = next_issue_revisions;
        state.workgraph_issue_state_fingerprints = next_issue_state_fingerprints;
        state.workgraph_issue_database_ids = next_issue_database_ids;
        state.workgraph_artifacts = next_artifacts;
        state.workgraph_artifact_revisions = next_artifact_revisions;
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

    pub(crate) async fn latest_workgraph_root_comment_revision(
        &self,
        source_key: &str,
    ) -> AnyResult<Option<RootIssueCommentRevisionState>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state
            .workgraph_root_comment_revisions
            .get(source_key)
            .map(|revision| RootIssueCommentRevisionState {
                document: state.workgraph_root_comments.get(source_key).cloned(),
                identity: state
                    .workgraph_root_comments
                    .get(source_key)
                    .map(RootIssueCommentIdentity::from)
                    .or_else(|| {
                        state
                            .workgraph_root_comment_tombstones
                            .get(source_key)
                            .cloned()
                    })
                    .expect("validated Root Issue comment revision has identity"),
                revision: *revision,
                fingerprint: state.workgraph_root_comment_fingerprints[source_key].clone(),
                tombstone: state
                    .workgraph_root_comment_tombstones
                    .contains_key(source_key),
            }))
    }

    pub(crate) async fn latest_workgraph_lifecycle_artifact_revision(
        &self,
        source_key: &str,
    ) -> AnyResult<Option<LifecycleArtifactRevisionState>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state
            .workgraph_artifact_revisions
            .get(source_key)
            .map(|revision| LifecycleArtifactRevisionState {
                document: state.workgraph_artifacts.get(source_key).cloned(),
                revision: *revision,
                tombstone: !state.workgraph_artifacts.contains_key(source_key),
            }))
    }

    /// The open lifecycle subject one GitHub account may answer on a task.
    ///
    /// A worker answers the Dispatch of the lease it holds, matched against
    /// the actor metadata that lease was *acquired* with. That snapshot is
    /// what lets an in-flight lease survive a catalog rename or a node ID
    /// re-encoding. An assigner answers the AssignmentRequest that named it,
    /// and only the one actor it named: the catalog must map that exact
    /// `assignerId` to this GitHub account. Everyone else answers a Result
    /// still awaiting its Evaluation, matched against the *current* catalog,
    /// because an evaluator holds no lease and so has no snapshot of its own.
    /// A task with no open subject has nothing for a human to respond to.
    ///
    /// The order is the order the questions were asked. A pending request is
    /// retired by the Assignment that answers it, and a Result exists only
    /// downstream of an Assignment, so an unanswered request is always the
    /// later, still-open question when both are present.
    pub(crate) async fn workgraph_task_response_subject(
        &self,
        task_source_key: &str,
        author_database_id: u64,
    ) -> AnyResult<Option<(String, TaskResponseSubject)>> {
        if author_database_id == 0 {
            return Ok(None);
        }
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        let own_lease = state.workgraph_active.values().find(|lease| {
            lease.task_source_key == task_source_key
                && !lease.completed
                && lease
                    .actor_github
                    .as_ref()
                    .is_some_and(|github| github.database_id == author_database_id)
        });
        if let Some(lease) = own_lease {
            // Holding a lease makes this actor the worker, never the evaluator
            // of the Result its own attempt produced. The Dispatch that
            // authorizes the work must already exist.
            if !lease.has_dispatch || lease.dispatch_id.is_empty() {
                return Ok(None);
            }
            return Ok(Some((
                lease.executor_id.clone(),
                TaskResponseSubject::Worker {
                    dispatch_id: lease.dispatch_id.clone(),
                    lease_id: lease.lease_id.clone(),
                },
            )));
        }
        if let Some(pending) = state.workgraph_pending_requests.get(task_source_key) {
            // Exactly one actor was asked, so exactly one may answer. The
            // catalog must currently map that `assignerId` to this GitHub
            // account: a request never confers authority on anyone else, and
            // a decision is not owed to a stale account either.
            if state.agents.get(&pending.assigner_id).is_some_and(|agent| {
                agent.configured
                    && agent.kind == ActorKind::Human
                    && agent
                        .github
                        .as_ref()
                        .is_some_and(|github| github.database_id == author_database_id)
            }) {
                return Ok(Some((
                    pending.assigner_id.clone(),
                    TaskResponseSubject::Assigner {
                        request_id: pending.request_id.clone(),
                    },
                )));
            }
        }
        let Some(pending) = state.workgraph_pending_results.get(task_source_key) else {
            return Ok(None);
        };
        let Some(actor_id) = state
            .agents
            .values()
            .find(|agent| {
                agent.configured
                    && agent.kind == ActorKind::Human
                    && agent
                        .github
                        .as_ref()
                        .is_some_and(|github| github.database_id == author_database_id)
            })
            .map(|agent| agent.agent_id.clone())
        else {
            return Ok(None);
        };
        // Nobody reviews their own work. The producing worker is recorded on
        // the pending Result itself, so this holds after their lease has
        // expired, been released, been cancelled with the task, or been
        // pruned by a restart — the lease's liveness is irrelevant.
        if pending.produced_by(author_database_id, &actor_id) {
            return Ok(None);
        }
        Ok(Some((
            actor_id,
            TaskResponseSubject::Evaluator {
                result_id: pending.result_id.clone(),
            },
        )))
    }

    /// The projected identity of one WorkGraph task Issue.
    ///
    /// Returns `(task_id, root_issue_id, workflow_run_id)`, which the Source
    /// validated when the task was admitted, so a natural response can be
    /// bound to a task without re-parsing the task body.
    pub(crate) async fn workgraph_task_identity(
        &self,
        task_source_key: &str,
    ) -> AnyResult<Option<(String, String, String)>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state
            .workgraph_task_identities
            .get(task_source_key)
            .map(|task| {
                (
                    task.task_id.clone(),
                    task.root_issue_id.clone(),
                    task.workflow_run_id.clone(),
                )
            }))
    }

    /// Return the latest revision observed for one natural task response,
    /// including a retained tombstone.
    pub(crate) async fn latest_workgraph_task_response_revision(
        &self,
        source_key: &str,
    ) -> AnyResult<Option<TaskResponseRevisionState>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        // `validate` proves a revision always pairs with a fingerprint and with
        // exactly one of a document or a tombstone, but this reads the maps
        // rather than trusting that: a restored state that somehow escaped the
        // check reports no prior revision instead of panicking.
        let (Some(revision), Some(fingerprint)) = (
            state.workgraph_task_response_revisions.get(source_key),
            state.workgraph_task_response_fingerprints.get(source_key),
        ) else {
            return Ok(None);
        };
        let document = state.workgraph_task_responses.get(source_key);
        let Some(identity) = document.map(TaskResponseIdentity::from).or_else(|| {
            state
                .workgraph_task_response_tombstones
                .get(source_key)
                .cloned()
        }) else {
            return Ok(None);
        };
        Ok(Some(TaskResponseRevisionState {
            tombstone: document.is_none(),
            document: document.cloned(),
            identity,
            revision: *revision,
            fingerprint: fingerprint.clone(),
        }))
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

    /// Return the canonical state fingerprint paired with the latest Issue revision.
    pub async fn latest_workgraph_issue_state_fingerprint(
        &self,
        source_key: &str,
    ) -> AnyResult<Option<String>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state
            .workgraph_issue_state_fingerprints
            .get(source_key)
            .cloned())
    }

    /// Resolve a numeric GitHub database issue ID to its GraphQL node ID.
    pub async fn workgraph_issue_node_id(
        &self,
        issue_database_id: u64,
    ) -> AnyResult<Option<String>> {
        let _guard = self.gate.lock().await;
        let state = self.ready_state().await?;
        Ok(state
            .workgraph_issue_database_ids
            .get(&issue_database_id)
            .cloned())
    }
}

fn projector_inputs(inputs: &[ProjectionInput]) -> Vec<ProjectionInput> {
    inputs
        .iter()
        .filter(|input| {
            !matches!(
                input,
                ProjectionInput::UpsertGitHubIssue(_) | ProjectionInput::DeleteGitHubIssue { .. }
            )
        })
        .cloned()
        .collect()
}

fn stage_workgraph_documents(
    state: &AllocationState,
    inputs: &[ProjectionInput],
) -> (
    BTreeMap<String, RootIssueDocument>,
    BTreeMap<String, i64>,
    BTreeMap<String, String>,
    BTreeMap<u64, String>,
    BTreeMap<String, TaskDocument>,
    BTreeMap<String, LifecycleArtifactDocument>,
    BTreeMap<String, i64>,
) {
    let mut root_issues = state.workgraph_root_issues.clone();
    let mut issue_revisions = state.workgraph_issue_revisions.clone();
    let mut issue_state_fingerprints = state.workgraph_issue_state_fingerprints.clone();
    let mut issue_database_ids = state.workgraph_issue_database_ids.clone();
    let mut tasks = state.workgraph_tasks.clone();
    let mut artifacts = state.workgraph_artifacts.clone();
    let mut artifact_revisions = state.workgraph_artifact_revisions.clone();
    apply_workgraph_documents(
        inputs,
        &mut root_issues,
        &mut issue_revisions,
        &mut issue_state_fingerprints,
        &mut issue_database_ids,
        &mut tasks,
        &mut artifacts,
        &mut artifact_revisions,
    );
    (
        root_issues,
        issue_revisions,
        issue_state_fingerprints,
        issue_database_ids,
        tasks,
        artifacts,
        artifact_revisions,
    )
}

type RootCommentDocuments = (
    BTreeMap<String, RootIssueCommentDocument>,
    BTreeMap<String, i64>,
    BTreeMap<String, String>,
    BTreeMap<String, RootIssueCommentIdentity>,
);

type TaskResponseDocuments = (
    BTreeMap<String, TaskResponseDocument>,
    BTreeMap<String, i64>,
    BTreeMap<String, String>,
    BTreeMap<String, TaskResponseIdentity>,
);

fn stage_root_comment_documents(
    state: &AllocationState,
    inputs: &[ProjectionInput],
) -> AnyResult<RootCommentDocuments> {
    let mut comments = state.workgraph_root_comments.clone();
    let mut revisions = state.workgraph_root_comment_revisions.clone();
    let mut fingerprints = state.workgraph_root_comment_fingerprints.clone();
    let mut tombstones = state.workgraph_root_comment_tombstones.clone();
    for input in inputs {
        match input {
            ProjectionInput::UpsertRootIssueComment(document) => {
                let fingerprint = root_comment_fingerprint(document)?;
                comments.insert(document.source_key.clone(), document.clone());
                revisions.insert(document.source_key.clone(), document.updated_at_revision);
                fingerprints.insert(document.source_key.clone(), fingerprint);
                tombstones.remove(&document.source_key);
            }
            ProjectionInput::DeleteRootIssueComment {
                source_key,
                root_issue_id,
                admission_id,
                repository_owner,
                repository_name,
                repository_node_id,
                issue_number,
                updated_at_revision,
            } => {
                comments.remove(source_key);
                revisions.insert(source_key.clone(), *updated_at_revision);
                fingerprints.insert(
                    source_key.clone(),
                    root_comment_tombstone_fingerprint(root_issue_id, admission_id),
                );
                tombstones.insert(
                    source_key.clone(),
                    RootIssueCommentIdentity {
                        source_key: source_key.clone(),
                        root_issue_id: root_issue_id.clone(),
                        admission_id: admission_id.clone(),
                        repository_owner: repository_owner.clone(),
                        repository_name: repository_name.clone(),
                        repository_node_id: repository_node_id.clone(),
                        issue_number: *issue_number,
                    },
                );
            }
            _ => {}
        }
    }
    Ok((comments, revisions, fingerprints, tombstones))
}

pub(crate) fn root_comment_fingerprint(document: &RootIssueCommentDocument) -> AnyResult<String> {
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(document).context("failed to fingerprint Root Issue comment")?,
    )))
}

/// Stages every authenticated task response one delivery carries.
///
/// A response is fenced exactly like a Root Issue comment: its revision and
/// content fingerprint are retained after retraction so a delayed or replayed
/// delivery cannot resurrect it.
fn stage_task_response_documents(
    state: &AllocationState,
    inputs: &[ProjectionInput],
) -> AnyResult<TaskResponseDocuments> {
    let mut responses = state.workgraph_task_responses.clone();
    let mut revisions = state.workgraph_task_response_revisions.clone();
    let mut fingerprints = state.workgraph_task_response_fingerprints.clone();
    let mut tombstones = state.workgraph_task_response_tombstones.clone();
    for input in inputs {
        match input {
            ProjectionInput::UpsertTaskResponse(document) => {
                let fingerprint = task_response_fingerprint(document)?;
                responses.insert(document.source_key.clone(), document.clone());
                revisions.insert(document.source_key.clone(), document.updated_at_revision);
                fingerprints.insert(document.source_key.clone(), fingerprint);
                tombstones.remove(&document.source_key);
            }
            ProjectionInput::DeleteTaskResponse {
                source_key,
                task_source_key,
                task_id,
                actor_id,
                updated_at_revision,
            } => {
                responses.remove(source_key);
                revisions.insert(source_key.clone(), *updated_at_revision);
                fingerprints.insert(
                    source_key.clone(),
                    task_response_tombstone_fingerprint(task_id, actor_id),
                );
                tombstones.insert(
                    source_key.clone(),
                    TaskResponseIdentity {
                        source_key: source_key.clone(),
                        task_source_key: task_source_key.clone(),
                        task_id: task_id.clone(),
                        actor_id: actor_id.clone(),
                    },
                );
            }
            _ => {}
        }
    }
    Ok((responses, revisions, fingerprints, tombstones))
}

/// Fingerprints the immutable GitHub evidence one task response carries.
///
/// Deliberately excludes the lifecycle subject and the actor it resolved to.
/// Those are allocator-derived and advance on their own, so folding them in
/// would make an identical GitHub redelivery look like same-revision comment
/// divergence every time the lifecycle moved. What this covers is exactly what
/// GitHub reported about the comment, which is what a revision fences.
pub(crate) fn task_response_fingerprint(document: &TaskResponseDocument) -> AnyResult<String> {
    let evidence = serde_json::json!({
        "sourceKey": document.source_key,
        "taskSourceKey": document.task_source_key,
        "authorDatabaseId": document.author_database_id,
        "authorId": document.author_id,
        "authorLogin": document.author_login,
        "body": document.body,
        "bodyDigest": document.body_digest,
        "createdAtRevision": document.created_at_revision,
        "updatedAtRevision": document.updated_at_revision,
    });
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&evidence).context("failed to fingerprint task response")?,
    )))
}

fn task_response_tombstone_fingerprint(task_id: &str, actor_id: &str) -> String {
    hex::encode(Sha256::digest(
        format!("deleted\0{task_id}\0{actor_id}").as_bytes(),
    ))
}

fn root_comment_tombstone_fingerprint(root_issue_id: &str, admission_id: &str) -> String {
    hex::encode(Sha256::digest(
        format!("deleted\0{root_issue_id}\0{admission_id}").as_bytes(),
    ))
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
                    updated_at_revision: document.updated_at_revision,
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
    issue_state_fingerprints: &mut BTreeMap<String, String>,
    issue_database_ids: &mut BTreeMap<u64, String>,
    tasks: &mut BTreeMap<String, TaskDocument>,
    artifacts: &mut BTreeMap<String, LifecycleArtifactDocument>,
    artifact_revisions: &mut BTreeMap<String, i64>,
) {
    for input in inputs {
        match input {
            ProjectionInput::RecordIssueRevision {
                source_key,
                revision,
                state_fingerprint,
                ..
            } => {
                issue_revisions.insert(source_key.clone(), *revision);
                issue_state_fingerprints.insert(source_key.clone(), state_fingerprint.clone());
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
            ProjectionInput::UpsertLocator(locator) => {
                issue_database_ids.insert(locator.issue_database_id, locator.source_key.clone());
            }
            ProjectionInput::DeleteLocator { source_key } => {
                issue_database_ids.retain(|_, value| value != source_key);
            }
            ProjectionInput::UpsertLifecycleArtifact(document) => {
                let current = artifact_revisions.get(&document.source_key).copied();
                let apply = current.is_none_or(|revision| {
                    document.updated_at_revision > revision
                        || document.updated_at_revision == revision
                            && artifacts.get(&document.source_key) == Some(document)
                });
                if apply {
                    artifacts.insert(document.source_key.clone(), document.clone());
                    artifact_revisions
                        .insert(document.source_key.clone(), document.updated_at_revision);
                }
            }
            ProjectionInput::DeleteLifecycleArtifact {
                source_key,
                updated_at_revision,
            } => {
                if artifact_revisions
                    .get(source_key)
                    .is_none_or(|revision| updated_at_revision >= revision)
                {
                    artifacts.remove(source_key);
                    artifact_revisions.insert(source_key.clone(), *updated_at_revision);
                }
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
                && valid_workgraph_id(&task.root_issue_id)
                && valid_typed_workgraph_id(&task.workflow_run_id, "workflow-run")
                && valid_typed_workgraph_id(&task.task_id, "task")
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

    // An AssignmentRequest is validated before the Assignments, because a
    // decision-bound Assignment must name a request this same projection
    // published. It is an action with no allocator authority: it is proven
    // against its own signed artifact and its task binding, and nothing else
    // in the allocator ever reads it.
    let mut request_sources = BTreeSet::new();
    let mut request_ids = BTreeSet::new();
    let mut requested_tasks = BTreeSet::new();
    for request in &projection.assignment_requests {
        let document = artifacts
            .get(&request.source_key)
            .context("WorkGraph AssignmentRequest has no authenticated artifact")?;
        let task = task_bindings
            .get(request.task_source_key.as_str())
            .context("WorkGraph AssignmentRequest has no accepted task binding")?;
        anyhow::ensure!(
            request_sources.insert(&request.source_key)
                && request_ids.insert(&request.request_id)
                // One task asks at most one open question at a time, so a
                // second request on the same Issue is ambiguous rather than
                // additive.
                && requested_tasks.insert(&request.task_source_key)
                && document.task_source_key == request.task_source_key
                && document
                    .body
                    .starts_with(WORKGRAPH_ASSIGNMENT_REQUEST_MARKER)
                && task.root_issue_id == request.root_issue_id
                && task.workflow_run_id == request.workflow_run_id
                && task.task_id == request.task_id
                && valid_typed_workgraph_id(&request.request_id, "assignment-request")
                && valid_workgraph_id(&request.assigner_id)
                && !request.candidates.is_empty()
                && request.candidates.len() <= MAX_WORKGRAPH_PERMITTED_EXECUTORS
                && request
                    .candidates
                    .iter()
                    .all(|candidate| valid_workgraph_id(candidate))
                && request.candidates.windows(2).all(|pair| pair[0] < pair[1])
                // Nobody hands work to themselves: the assigner decides among
                // others or the request is not a decision at all.
                && !request.candidates.contains(&request.assigner_id),
            "WorkGraph allocator projection contains an invalid or duplicate AssignmentRequest"
        );
    }

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
                && document.body.starts_with(WORKGRAPH_ASSIGNMENT_MARKER)
                && task.root_issue_id == assignment.root_issue_id
                && task.workflow_run_id == assignment.workflow_run_id
                && task.task_id == assignment.task_id
                && valid_typed_workgraph_id(&assignment.assignment_id, "assignment")
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
        anyhow::ensure!(
            valid_assignment_decision(assignment, &projection.assignment_requests),
            "WorkGraph allocator projection contains an incoherent assignment decision"
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
                && assignment.root_issue_id == dispatch.root_issue_id
                && assignment.workflow_run_id == dispatch.workflow_run_id
                && assignment.task_id == dispatch.task_id
                && assignment
                    .permitted_executors
                    .contains(&dispatch.executor_id)
                && valid_typed_workgraph_id(&dispatch.task_id, "task")
                && valid_workgraph_id(&dispatch.root_issue_id)
                && valid_typed_workgraph_id(&dispatch.workflow_run_id, "workflow-run")
                && valid_typed_workgraph_id(&dispatch.assignment_id, "assignment")
                && valid_typed_workgraph_id(&dispatch.lease_id, "lease")
                && valid_dispatch_identity(&dispatch.dispatch_id)
                && valid_workgraph_id(&dispatch.executor_id)
                && valid_workgraph_id(&dispatch.slot_id),
            "WorkGraph allocator projection contains an invalid or duplicate dispatch"
        );
    }

    let dispatches_by_lease = projection
        .dispatches
        .iter()
        .map(|dispatch| (dispatch.lease_id.as_str(), dispatch))
        .collect::<BTreeMap<_, _>>();
    let mut result_sources = BTreeSet::new();
    let mut result_ids = BTreeSet::new();
    let mut result_leases = BTreeSet::new();
    for result in &projection.results {
        let document = artifacts
            .get(&result.source_key)
            .context("WorkGraph Result has no authenticated artifact")?;
        let task = task_bindings
            .get(result.task_source_key.as_str())
            .context("WorkGraph Result has no accepted task binding")?;
        let dispatch = dispatches_by_lease
            .get(result.lease_id.as_str())
            .context("WorkGraph Result has no selected Dispatch")?;
        anyhow::ensure!(
            result_sources.insert(&result.source_key)
                && result_ids.insert(&result.result_id)
                && result_leases.insert(&result.lease_id)
                && document.task_source_key == result.task_source_key
                && document.body.starts_with(WORKGRAPH_RESULT_MARKER)
                && task.root_issue_id == result.root_issue_id
                && task.workflow_run_id == result.workflow_run_id
                && task.task_id == result.task_id
                && dispatch.task_source_key == result.task_source_key
                && dispatch.root_issue_id == result.root_issue_id
                && dispatch.workflow_run_id == result.workflow_run_id
                && dispatch.task_id == result.task_id
                && result.attempt > 0
                && result.attempt <= MAX_WORKGRAPH_ATTEMPTS
                && result.lease_id
                    == make_lease_id(&result.task_id, &dispatch.assignment_id, result.attempt,)
                && valid_workgraph_id(&result.root_issue_id)
                && valid_typed_workgraph_id(&result.workflow_run_id, "workflow-run")
                && valid_typed_workgraph_id(&result.task_id, "task")
                && valid_typed_workgraph_id(&result.result_id, "result")
                && valid_typed_workgraph_id(&result.lease_id, "lease"),
            "WorkGraph allocator projection contains an invalid or duplicate Result"
        );
    }

    let results_by_id = projection
        .results
        .iter()
        .map(|result| (result.result_id.as_str(), result))
        .collect::<BTreeMap<_, _>>();
    let mut evaluation_sources = BTreeSet::new();
    let mut evaluation_ids = BTreeSet::new();
    let mut evaluated_results = BTreeSet::new();
    for evaluation in &projection.evaluations {
        let document = artifacts
            .get(&evaluation.source_key)
            .context("WorkGraph Evaluate has no authenticated artifact")?;
        let task = task_bindings
            .get(evaluation.task_source_key.as_str())
            .context("WorkGraph Evaluate has no accepted task binding")?;
        let result = results_by_id
            .get(evaluation.result_id.as_str())
            .context("WorkGraph Evaluate has no selected Result")?;
        anyhow::ensure!(
            evaluation_sources.insert(&evaluation.source_key)
                && evaluation_ids.insert(&evaluation.evaluation_id)
                && evaluated_results.insert(&evaluation.result_id)
                && document.task_source_key == evaluation.task_source_key
                && document.body.starts_with(WORKGRAPH_EVALUATION_MARKER)
                && task.root_issue_id == evaluation.root_issue_id
                && task.workflow_run_id == evaluation.workflow_run_id
                && task.task_id == evaluation.task_id
                && result.task_source_key == evaluation.task_source_key
                && result.root_issue_id == evaluation.root_issue_id
                && result.workflow_run_id == evaluation.workflow_run_id
                && result.task_id == evaluation.task_id
                && result.attempt == evaluation.attempt
                && matches!(
                    evaluation.verdict.as_str(),
                    WORKGRAPH_EVALUATION_ACCEPTED | WORKGRAPH_EVALUATION_REJECTED
                )
                && valid_workgraph_id(&evaluation.root_issue_id)
                && valid_typed_workgraph_id(&evaluation.workflow_run_id, "workflow-run")
                && valid_typed_workgraph_id(&evaluation.task_id, "task")
                && valid_typed_workgraph_id(&evaluation.result_id, "result")
                && valid_typed_workgraph_id(&evaluation.evaluation_id, "evaluation")
                && valid_workgraph_id(&evaluation.verdict),
            "WorkGraph allocator projection contains an invalid or duplicate Evaluate"
        );
    }

    let evaluations_by_id = projection
        .evaluations
        .iter()
        .map(|evaluation| (evaluation.evaluation_id.as_str(), evaluation))
        .collect::<BTreeMap<_, _>>();
    let mut route_sources = BTreeSet::new();
    let mut route_ids = BTreeSet::new();
    let mut routed_evaluations = BTreeSet::new();
    let mut routed_tasks = BTreeSet::new();
    for route in &projection.routes {
        let document = artifacts
            .get(&route.source_key)
            .context("WorkGraph Route has no authenticated artifact")?;
        let task = task_bindings
            .get(route.task_source_key.as_str())
            .context("WorkGraph Route has no accepted task binding")?;
        let result = results_by_id
            .get(route.result_id.as_str())
            .context("WorkGraph Route has no selected Result")?;
        let evaluation = evaluations_by_id
            .get(route.evaluation_id.as_str())
            .context("WorkGraph Route has no selected Evaluation")?;
        let rework = route.action == WORKGRAPH_ROUTE_REWORK;
        anyhow::ensure!(
            route_sources.insert(&route.source_key)
                && route_ids.insert(&route.route_id)
                && routed_evaluations.insert(&route.evaluation_id)
                && routed_tasks.insert(&route.task_source_key)
                && document.task_source_key == route.task_source_key
                && document.body.starts_with(WORKGRAPH_ROUTE_MARKER)
                && task.root_issue_id == route.root_issue_id
                && task.workflow_run_id == route.workflow_run_id
                && task.task_id == route.task_id
                && result.task_source_key == route.task_source_key
                && result.root_issue_id == route.root_issue_id
                && result.workflow_run_id == route.workflow_run_id
                && result.task_id == route.task_id
                && result.attempt == route.attempt
                && evaluation.task_source_key == route.task_source_key
                && evaluation.root_issue_id == route.root_issue_id
                && evaluation.workflow_run_id == route.workflow_run_id
                && evaluation.task_id == route.task_id
                && evaluation.result_id == route.result_id
                && evaluation.attempt == route.attempt
                && (!rework || evaluation.verdict == WORKGRAPH_EVALUATION_REJECTED)
                && (evaluation.verdict != WORKGRAPH_EVALUATION_ACCEPTED || !rework)
                && route.attempt > 0
                && route.max_attempts > 0
                && route.max_attempts <= MAX_WORKGRAPH_ATTEMPTS
                && route.attempt <= route.max_attempts
                && (!rework || route.attempt < route.max_attempts)
                && valid_workgraph_id(&route.root_issue_id)
                && valid_typed_workgraph_id(&route.workflow_run_id, "workflow-run")
                && valid_typed_workgraph_id(&route.task_id, "task")
                && valid_typed_workgraph_id(&route.result_id, "result")
                && valid_typed_workgraph_id(&route.evaluation_id, "evaluation")
                && valid_typed_workgraph_id(&route.route_id, "route")
                && valid_workgraph_id(&route.action),
            "WorkGraph allocator projection contains an invalid, stale, or duplicate Route"
        );
    }
    Ok(())
}

fn valid_workgraph_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_WORKGRAPH_ID_LENGTH
}

fn valid_typed_workgraph_id(value: &str, id_type: &str) -> bool {
    value.len() <= MAX_WORKGRAPH_ID_LENGTH && is_typed_workgraph_id(value, id_type)
}

/// A Root Issue's active mapping admission set must be ordered by mapping ID,
/// unique by mapping ID *and* by selector label, carry a typed admission ID per
/// activation, and never name a reserved exclusion modifier as its selector.
fn valid_root_mapping_admissions(
    mappings: &[crate::protocol::RootMappingAdmission],
    admission_id: &str,
) -> bool {
    if mappings.is_empty() {
        return false;
    }
    if mappings
        .windows(2)
        .any(|pair| pair[0].mapping_id >= pair[1].mapping_id)
    {
        return false;
    }
    let labels: BTreeSet<&str> = mappings
        .iter()
        .map(|mapping| mapping.label.as_str())
        .collect();
    if labels.len() != mappings.len() {
        return false;
    }
    if crate::protocol::RootIssueDocument::legacy_admission_id(mappings) != Some(admission_id) {
        return false;
    }
    mappings.iter().all(|mapping| {
        valid_typed_workgraph_id(&mapping.admission_id, "admission")
            && valid_workgraph_id(&mapping.mapping_id)
            && valid_workgraph_id(&mapping.label)
            && !matches!(
                mapping.label.as_str(),
                crate::protocol::WORKGRAPH_IGNORE_LABEL | crate::protocol::WORKGRAPH_ERROR_LABEL
            )
            && (mapping.label == crate::protocol::WORKGRAPH_ADMISSION_LABEL
                || mapping
                    .label
                    .starts_with(crate::protocol::WORKGRAPH_LABEL_PREFIX))
            && valid_workgraph_id(&mapping.definition_repository)
            && mapping.definition_repository.split('/').count() == 2
            && valid_workgraph_id(&mapping.definition_ref)
            && valid_workgraph_id(&mapping.definition_path)
    })
}

/// A Root Issue comment's admission set must be ordered, unique, typed, and
/// must contain the compatibility `admission_id` it was stamped with.
///
/// An empty set is a legacy comment that predates admission sets: its single
/// admission is the compatibility `admission_id`, which is validated
/// separately.
/// Whether a task response names exactly the lifecycle subject its role
/// answers, so a stored document can never carry a contradictory binding.
/// Whether a projected or recorded Dispatch identity is admissible.
///
/// Empty is the rollout state: a projector that does not publish canonical
/// Dispatch identities yet, and a lease recorded before it did. Anything else
/// must be a canonical typed `dispatch` ID, so a malformed identity is refused
/// at admission rather than persisted onto a lease and later bound into a
/// worker response.
fn valid_dispatch_identity(dispatch_id: &str) -> bool {
    dispatch_id.is_empty() || valid_typed_workgraph_id(dispatch_id, "dispatch")
}

/// Whether a lease's pinned actor metadata is internally well formed.
///
/// This is a shape check on the acquisition snapshot, never a comparison with
/// the current catalog. A human lease must carry the exact GitHub account it
/// was granted to, keyed on the numeric ID that survives renames and both node
/// ID encodings; an agent lease carries no account at all.
fn valid_lease_actor_snapshot(lease: &WorkGraphActiveLease) -> bool {
    match lease.actor_kind {
        ActorKind::Agent => lease.actor_github.is_none(),
        ActorKind::Human => lease.actor_github.as_ref().is_some_and(|github| {
            github.database_id > 0
                && !github.node_id.trim().is_empty()
                && !github.login.trim().is_empty()
        }),
    }
}

fn valid_task_response_subject(response: &TaskResponseDocument) -> bool {
    match response.role {
        TaskResponseRole::Worker => {
            response.result_id.is_none()
                && response.request_id.is_none()
                && response
                    .dispatch_id
                    .as_ref()
                    .is_some_and(|id| valid_typed_workgraph_id(id, "dispatch"))
                && response
                    .lease_id
                    .as_ref()
                    .is_some_and(|id| valid_typed_workgraph_id(id, "lease"))
        }
        TaskResponseRole::Assigner => {
            response.dispatch_id.is_none()
                && response.lease_id.is_none()
                && response.result_id.is_none()
                && response
                    .request_id
                    .as_ref()
                    .is_some_and(|id| valid_typed_workgraph_id(id, "assignment-request"))
        }
        TaskResponseRole::Evaluator => {
            response.dispatch_id.is_none()
                && response.lease_id.is_none()
                && response.request_id.is_none()
                && response
                    .result_id
                    .as_ref()
                    .is_some_and(|id| valid_typed_workgraph_id(id, "result"))
        }
    }
}

/// Whether an Assignment's optional decision provenance is coherent.
///
/// A legacy Assignment names no request, no response, and no assigner, and is
/// admitted exactly as it always was. A decision-bound Assignment must name
/// the request it answers *and* the assigner that answered it, that request
/// must be one this projection published against the same task, the selected
/// executor set must be the single candidate the request offered, and the
/// assigner may never select itself. The human Response the decision was read
/// from is optional: an agent assigner decides without a comment.
fn valid_assignment_decision(
    assignment: &WorkGraphAssignmentBinding,
    requests: &[WorkGraphAssignmentRequestBinding],
) -> bool {
    let (Some(request_id), Some(assigner_id)) = (&assignment.request_id, &assignment.assigner_id)
    else {
        return assignment.request_id.is_none()
            && assignment.assigner_id.is_none()
            && assignment.response_id.is_none();
    };
    let Some(request) = requests
        .iter()
        .find(|request| request.request_id == *request_id)
    else {
        return false;
    };
    valid_typed_workgraph_id(request_id, "assignment-request")
        && valid_workgraph_id(assigner_id)
        && request.task_source_key == assignment.task_source_key
        && request.task_id == assignment.task_id
        && request.assigner_id == *assigner_id
        && assignment.permitted_executors.len() == 1
        && request
            .candidates
            .contains(&assignment.permitted_executors[0])
        && assignment.permitted_executors[0] != *assigner_id
        && assignment
            .response_id
            .as_ref()
            .is_none_or(|response_id| valid_typed_workgraph_id(response_id, "response"))
}

fn valid_root_comment_admission_ids(comment: &RootIssueCommentDocument) -> bool {
    if comment.admission_ids.is_empty() {
        return true;
    }
    comment
        .admission_ids
        .windows(2)
        .all(|pair| pair[0] < pair[1])
        && comment
            .admission_ids
            .iter()
            .all(|admission_id| valid_typed_workgraph_id(admission_id, "admission"))
        && comment.admission_ids.contains(&comment.admission_id)
}

fn valid_workgraph_inclusion(labels: &[String], included: bool) -> bool {
    labels.iter().all(|label| label.starts_with("workgraph:"))
        && labels.windows(2).all(|pair| pair[0] <= pair[1])
        && included
            == !labels
                .iter()
                .any(|label| matches!(label.as_str(), "workgraph:ignore" | "workgraph:error"))
}

fn workgraph_assignment_matches(
    state: &WorkGraphAssignmentState,
    desired: &WorkGraphAssignmentBinding,
) -> bool {
    state.task_source_key == desired.task_source_key
        && state.root_issue_id == desired.root_issue_id
        && state.workflow_run_id == desired.workflow_run_id
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
        && lease.root_issue_id == desired.root_issue_id
        && lease.workflow_run_id == desired.workflow_run_id
        && lease.task_id == desired.task_id
        && lease.assignment_id == desired.assignment_id
        && lease.lease_id == desired.lease_id
        && lease.executor_id == desired.executor_id
        && lease.slot_id == desired.slot_id
        // A missing Dispatch identity on either side is a rollout gap, not a
        // different Dispatch: a projector that does not publish one yet, and a
        // lease recorded before it did, both still match and are backfilled.
        // Two *different* non-empty identities are genuinely different
        // Dispatches, so the recorded one is retracted instead.
        && (lease.dispatch_id.is_empty()
            || desired.dispatch_id.is_empty()
            || lease.dispatch_id == desired.dispatch_id)
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
            workgraph_routes: BTreeMap::new(),
            pending: Vec::new(),
            pending_offset: 0,
            workgraph_checkpoint: Vec::new(),
            workgraph_tasks: BTreeMap::new(),
            workgraph_root_issues: BTreeMap::new(),
            workgraph_root_comments: BTreeMap::new(),
            workgraph_root_comment_revisions: BTreeMap::new(),
            workgraph_root_comment_fingerprints: BTreeMap::new(),
            workgraph_root_comment_tombstones: BTreeMap::new(),
            workgraph_task_responses: BTreeMap::new(),
            workgraph_task_response_revisions: BTreeMap::new(),
            workgraph_task_response_fingerprints: BTreeMap::new(),
            workgraph_task_response_tombstones: BTreeMap::new(),
            workgraph_pending_results: BTreeMap::new(),
            workgraph_pending_requests: BTreeMap::new(),
            workgraph_issue_revisions: BTreeMap::new(),
            workgraph_issue_state_fingerprints: BTreeMap::new(),
            workgraph_issue_database_ids: BTreeMap::new(),
            workgraph_artifacts: BTreeMap::new(),
            workgraph_artifact_revisions: BTreeMap::new(),
            workgraph_authorizations: BTreeMap::new(),
            workgraph_artifact_generations: BTreeMap::new(),
            workgraph_stale_authorizations: BTreeSet::new(),
            pending_workgraph_origins: BTreeSet::new(),
        }
    }
}

impl AllocationState {
    fn refresh_workgraph_authorizations(
        &mut self,
        projection: &WorkGraphAllocatorProjection,
        tasks: &BTreeMap<String, TaskDocument>,
        roots: &BTreeMap<String, RootIssueDocument>,
        issue_revisions: &BTreeMap<String, i64>,
        artifacts: &BTreeMap<String, LifecycleArtifactDocument>,
        inputs: &[ProjectionInput],
    ) {
        let explicit_transitions = inputs
            .iter()
            .filter_map(|input| match input {
                ProjectionInput::RecordIssueRevision {
                    source_key,
                    revision,
                    authorization_transition: true,
                    ..
                } => Some((source_key.as_str(), *revision)),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let desired = projection
            .tasks
            .iter()
            .map(|task| (task.source_key.as_str(), task))
            .collect::<BTreeMap<_, _>>();
        for source_key in self
            .workgraph_authorizations
            .keys()
            .filter(|source_key| !desired.contains_key(source_key.as_str()))
            .cloned()
            .collect::<Vec<_>>()
        {
            let authorization = self
                .workgraph_authorizations
                .get_mut(&source_key)
                .expect("selected authorization exists");
            let explicit_transition = explicit_transitions
                .get(source_key.as_str())
                .copied()
                .unwrap_or_default()
                .max(
                    explicit_transitions
                        .get(authorization.root_issue_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                );
            if authorization.included || explicit_transition > authorization.transition_revision {
                authorization.generation += 1;
                authorization.cutoff_revision = authorization.cutoff_revision.max(
                    issue_revisions
                        .get(&source_key)
                        .copied()
                        .unwrap_or_default()
                        .max(
                            issue_revisions
                                .get(&authorization.root_issue_id)
                                .copied()
                                .unwrap_or_default(),
                        ),
                );
                authorization.transition_revision =
                    authorization.transition_revision.max(explicit_transition);
                authorization.included = false;
            }
        }
        for binding in &projection.tasks {
            let task = tasks
                .get(&binding.source_key)
                .expect("projection provenance was validated");
            let included = task.workgraph_include
                && !roots
                    .get(&binding.root_issue_id)
                    .is_some_and(|root| !root.workgraph_include);
            let transition_revision = issue_revisions
                .get(&binding.source_key)
                .copied()
                .unwrap_or_default()
                .max(
                    issue_revisions
                        .get(&binding.root_issue_id)
                        .copied()
                        .unwrap_or_default(),
                );
            let explicit_transition = explicit_transitions
                .get(binding.source_key.as_str())
                .copied()
                .unwrap_or_default()
                .max(
                    explicit_transitions
                        .get(binding.root_issue_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                );
            match self.workgraph_authorizations.get_mut(&binding.source_key) {
                Some(authorization)
                    if authorization.included != included
                        || authorization.root_issue_id != binding.root_issue_id
                        || explicit_transition > authorization.transition_revision =>
                {
                    authorization.root_issue_id = binding.root_issue_id.clone();
                    authorization.generation += 1;
                    authorization.cutoff_revision =
                        authorization.cutoff_revision.max(transition_revision);
                    authorization.transition_revision =
                        authorization.transition_revision.max(explicit_transition);
                    authorization.included = included;
                }
                Some(_) => {}
                None => {
                    self.workgraph_authorizations.insert(
                        binding.source_key.clone(),
                        WorkGraphAuthorizationState {
                            root_issue_id: binding.root_issue_id.clone(),
                            generation: 1,
                            cutoff_revision: (!included || explicit_transition > 0)
                                .then_some(transition_revision)
                                .unwrap_or_default(),
                            transition_revision: explicit_transition,
                            included,
                        },
                    );
                }
            }
        }

        self.workgraph_artifact_generations.retain(|source_key, _| {
            inputs.iter().all(|input| {
                !matches!(
                    input,
                    ProjectionInput::DeleteLifecycleArtifact {
                        source_key: deleted,
                        ..
                    } if deleted == source_key
                )
            })
        });
        for artifact in artifacts
            .values()
            .filter(|artifact| {
                !self
                    .workgraph_artifact_generations
                    .get(&artifact.source_key)
                    .is_some_and(|generation| *generation > 0)
            })
            .cloned()
            .collect::<Vec<_>>()
        {
            let generation = self
                .workgraph_authorizations
                .get(&artifact.task_source_key)
                .filter(|authorization| {
                    authorization.included
                        && artifact.created_at_revision > authorization.cutoff_revision
                })
                .map(|authorization| authorization.generation)
                .unwrap_or_default();
            self.workgraph_artifact_generations
                .insert(artifact.source_key, generation);
        }
        for input in inputs {
            let ProjectionInput::UpsertLifecycleArtifact(artifact) = input else {
                continue;
            };
            let generation = self
                .workgraph_authorizations
                .get(&artifact.task_source_key)
                .filter(|authorization| {
                    authorization.included
                        && artifact.created_at_revision > authorization.cutoff_revision
                })
                .map(|authorization| authorization.generation)
                .unwrap_or_default();
            self.workgraph_artifact_generations
                .insert(artifact.source_key.clone(), generation);
        }
        self.workgraph_routes.retain(|_, route| {
            self.workgraph_authorizations
                .get(&route.task_source_key)
                .is_some_and(|authorization| {
                    authorization.included
                        && authorization.root_issue_id == route.root_issue_id
                        && authorization.generation == route.authorization_generation
                })
        });
    }

    fn fence_stale_workgraph_authorizations(&self, projection: &mut WorkGraphAllocatorProjection) {
        projection.assignment_requests.retain(|request| {
            self.artifact_is_current(&request.task_source_key, &request.source_key)
        });
        let request_ids = projection
            .assignment_requests
            .iter()
            .map(|request| request.request_id.as_str())
            .collect::<BTreeSet<_>>();
        projection.assignments.retain(|assignment| {
            assignment
                .request_id
                .as_deref()
                .is_none_or(|request_id| request_ids.contains(request_id))
                && self.artifact_is_current(&assignment.task_source_key, &assignment.source_key)
        });
        let assignment_ids = projection
            .assignments
            .iter()
            .map(|assignment| assignment.assignment_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut removed_leases = BTreeSet::new();
        projection.dispatches.retain(|dispatch| {
            let current = assignment_ids.contains(dispatch.assignment_id.as_str())
                && self
                    .workgraph_authorizations
                    .get(&dispatch.task_source_key)
                    .filter(|authorization| authorization.included)
                    .zip(
                        self.workgraph_artifact_generations
                            .get(&dispatch.source_key),
                    )
                    .is_some_and(|(authorization, artifact_generation)| {
                        authorization.generation == *artifact_generation
                    });
            if !current {
                removed_leases.insert(dispatch.lease_id.clone());
            }
            current
        });
        projection.results.retain(|result| {
            !removed_leases.contains(&result.lease_id)
                && self.artifact_is_current(&result.task_source_key, &result.source_key)
        });
        let result_ids = projection
            .results
            .iter()
            .map(|result| result.result_id.as_str())
            .collect::<BTreeSet<_>>();
        projection.evaluations.retain(|evaluation| {
            result_ids.contains(evaluation.result_id.as_str())
                && self.artifact_is_current(&evaluation.task_source_key, &evaluation.source_key)
        });
        let evaluation_ids = projection
            .evaluations
            .iter()
            .map(|evaluation| evaluation.evaluation_id.as_str())
            .collect::<BTreeSet<_>>();
        projection.routes.retain(|route| {
            result_ids.contains(route.result_id.as_str())
                && evaluation_ids.contains(route.evaluation_id.as_str())
                && self.artifact_is_current(&route.task_source_key, &route.source_key)
        });
    }

    fn artifact_is_current(&self, task_source_key: &str, source_key: &str) -> bool {
        self.workgraph_authorizations
            .get(task_source_key)
            .filter(|authorization| authorization.included)
            .zip(self.workgraph_artifact_generations.get(source_key))
            .is_some_and(|(authorization, artifact_generation)| {
                authorization.generation == *artifact_generation
            })
    }
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
                || !valid_workgraph_id(&task.root_issue_id)
                || !valid_typed_workgraph_id(&task.workflow_run_id, "workflow-run")
                || !valid_typed_workgraph_id(&task.task_id, "task")
                || task.task_element_id.trim().is_empty()
                || !canonical_task_ids.insert(&task.task_id)
                || !canonical_task_elements.insert(&task.task_element_id)
            {
                return Err("WorkGraph task identity state is invalid or duplicated".into());
            }
        }
        if self
            .workgraph_issue_database_ids
            .iter()
            .any(|(database_id, source_key)| {
                *database_id == 0
                    || source_key.trim().is_empty()
                    || !(self.workgraph_tasks.contains_key(source_key)
                        || self.workgraph_root_issues.contains_key(source_key))
            })
        {
            return Err("WorkGraph issue database ID index is invalid".into());
        }
        if self
            .workgraph_tasks
            .values()
            .any(|task| !valid_workgraph_inclusion(&task.workgraph_labels, task.workgraph_include))
            || self.workgraph_root_issues.values().any(|root| {
                !valid_workgraph_inclusion(&root.workgraph_labels, root.workgraph_include)
                    || !valid_typed_workgraph_id(&root.admission_id, "admission")
                    || !valid_root_mapping_admissions(&root.workflow_mappings, &root.admission_id)
            })
        {
            return Err("WorkGraph inclusion state is invalid".into());
        }
        if self
            .workgraph_root_comment_revisions
            .keys()
            .ne(self.workgraph_root_comment_fingerprints.keys())
            || self
                .workgraph_root_comments
                .iter()
                .any(|(source_key, comment)| {
                    comment.source_key != *source_key
                        || self
                            .workgraph_root_comment_tombstones
                            .contains_key(source_key)
                        || self
                            .workgraph_root_comment_revisions
                            .get(source_key)
                            .is_none_or(|revision| *revision != comment.updated_at_revision)
                        || comment.created_at_revision < 0
                        || comment.updated_at_revision < comment.created_at_revision
                        || comment.body.len() > MAX_ROOT_ISSUE_COMMENT_BODY_BYTES
                        || comment.issue_number == 0
                        || !valid_typed_workgraph_id(&comment.admission_id, "admission")
                        || !valid_root_comment_admission_ids(comment)
                        || [
                            &comment.source_key,
                            &comment.root_issue_id,
                            &comment.repository_owner,
                            &comment.repository_name,
                            &comment.repository_node_id,
                            &comment.author_id,
                            &comment.author_type,
                            &comment.author_login,
                        ]
                        .into_iter()
                        .any(|value| !valid_workgraph_id(value))
                })
            || self
                .workgraph_root_comment_tombstones
                .iter()
                .any(|(source_key, identity)| {
                    self.workgraph_root_comments.contains_key(source_key)
                        || !self
                            .workgraph_root_comment_revisions
                            .contains_key(source_key)
                        || identity.source_key != *source_key
                        || identity.issue_number == 0
                        || !valid_typed_workgraph_id(&identity.admission_id, "admission")
                        || [
                            &identity.source_key,
                            &identity.root_issue_id,
                            &identity.repository_owner,
                            &identity.repository_name,
                            &identity.repository_node_id,
                        ]
                        .into_iter()
                        .any(|value| !valid_workgraph_id(value))
                })
            || self
                .workgraph_root_comment_revisions
                .values()
                .any(|revision| *revision < 0)
            || self
                .workgraph_root_comment_fingerprints
                .values()
                .any(|fingerprint| fingerprint.len() != 64)
        {
            return Err("WorkGraph Root Issue comment revision state is invalid".into());
        }
        // Task responses carry the same four-map shape as Root Issue comments,
        // so they are proven the same way: every revision has a fingerprint,
        // every retained document or tombstone has a revision, and the two are
        // mutually exclusive. Every read of these maps below relies on it.
        if self
            .workgraph_task_response_revisions
            .keys()
            .ne(self.workgraph_task_response_fingerprints.keys())
            || self
                .workgraph_task_responses
                .iter()
                .any(|(source_key, response)| {
                    response.source_key != *source_key
                        || self
                            .workgraph_task_response_tombstones
                            .contains_key(source_key)
                        || self
                            .workgraph_task_response_revisions
                            .get(source_key)
                            .is_none_or(|revision| *revision != response.updated_at_revision)
                        || response.created_at_revision < 0
                        || response.updated_at_revision < response.created_at_revision
                        || response.body.trim().is_empty()
                        || response.body.len() > MAX_TASK_RESPONSE_BODY_BYTES
                        || response.body_digest
                            != derive_workgraph_response_body_digest(&response.body)
                        || response.author_database_id == 0
                        || !valid_typed_workgraph_id(&response.task_id, "task")
                        || !valid_typed_workgraph_id(&response.workflow_run_id, "workflow-run")
                        || !valid_task_response_subject(response)
                        || [
                            &response.source_key,
                            &response.task_source_key,
                            &response.actor_id,
                            &response.root_issue_id,
                            &response.author_id,
                            &response.author_login,
                        ]
                        .into_iter()
                        .any(|value| !valid_workgraph_id(value))
                })
            || self
                .workgraph_task_response_tombstones
                .iter()
                .any(|(source_key, identity)| {
                    self.workgraph_task_responses.contains_key(source_key)
                        || !self
                            .workgraph_task_response_revisions
                            .contains_key(source_key)
                        || identity.source_key != *source_key
                        || !valid_typed_workgraph_id(&identity.task_id, "task")
                        || [
                            &identity.source_key,
                            &identity.task_source_key,
                            &identity.actor_id,
                        ]
                        .into_iter()
                        .any(|value| !valid_workgraph_id(value))
                })
            || self
                .workgraph_task_response_revisions
                .values()
                .any(|revision| *revision < 0)
            || self
                .workgraph_task_response_fingerprints
                .values()
                .any(|fingerprint| fingerprint.len() != 64)
            || self
                .workgraph_pending_results
                .iter()
                .any(|(task_source_key, pending)| {
                    !valid_workgraph_id(task_source_key)
                        || !valid_typed_workgraph_id(&pending.result_id, "result")
                        || !valid_typed_workgraph_id(&pending.lease_id, "lease")
                        || pending.executor_id.is_empty()
                        || pending
                            .producer_github
                            .as_ref()
                            .is_some_and(|github| github.database_id == 0)
                })
            || self
                .workgraph_pending_requests
                .iter()
                .any(|(task_source_key, pending)| {
                    !valid_workgraph_id(task_source_key)
                        || !valid_workgraph_id(&pending.source_key)
                        || !valid_typed_workgraph_id(&pending.request_id, "assignment-request")
                        || !valid_workgraph_id(&pending.assigner_id)
                        || pending.candidates.is_empty()
                        || pending.candidates.len() > MAX_WORKGRAPH_PERMITTED_EXECUTORS
                        || !pending
                            .candidates
                            .iter()
                            .all(|candidate| valid_workgraph_id(candidate))
                        || !pending.candidates.windows(2).all(|pair| pair[0] < pair[1])
                        || pending.candidates.contains(&pending.assigner_id)
                })
        {
            return Err("WorkGraph task response revision state is invalid".into());
        }
        if self
            .workgraph_authorizations
            .iter()
            .any(|(source_key, authorization)| {
                !valid_workgraph_id(source_key)
                    || !valid_workgraph_id(&authorization.root_issue_id)
                    || authorization.generation == 0
                    || authorization.cutoff_revision < 0
                    || authorization.transition_revision < 0
                    || authorization.transition_revision > authorization.cutoff_revision
            })
            || self
                .workgraph_artifact_generations
                .keys()
                .any(|source_key| !self.workgraph_artifacts.contains_key(source_key))
            || self
                .workgraph_artifacts
                .iter()
                .any(|(source_key, artifact)| {
                    artifact.source_key != *source_key
                        || artifact.created_at_revision < 0
                        || artifact.updated_at_revision < artifact.created_at_revision
                        || self
                            .workgraph_artifact_revisions
                            .get(source_key)
                            .is_none_or(|revision| *revision != artifact.updated_at_revision)
                })
            || self
                .workgraph_artifact_revisions
                .values()
                .any(|revision| *revision < 0)
        {
            return Err("WorkGraph authorization generation state is invalid".into());
        }
        if self
            .workgraph_issue_state_fingerprints
            .keys()
            .ne(self.workgraph_issue_revisions.keys())
            || self
                .workgraph_issue_state_fingerprints
                .values()
                .any(|fingerprint| fingerprint.len() != 64)
        {
            return Err("WorkGraph Issue revision state is incomplete".into());
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
                || !valid_typed_workgraph_id(&assignment.assignment_id, "assignment")
                || assignment.task_id != task.task_id
                || assignment.root_issue_id != task.root_issue_id
                || assignment.workflow_run_id != task.workflow_run_id
                || ((!task.is_open || !task.workgraph_include) && assignment.eligible)
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
                || assignment.max_attempts == 0
                || assignment.max_attempts > MAX_WORKGRAPH_ATTEMPTS
                || assignment.next_attempt > assignment.max_attempts
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
                || lease.root_issue_id != task.root_issue_id
                || lease.workflow_run_id != task.workflow_run_id
                || lease.task_element_id != task.task_element_id
                || assignment.task_source_key != lease.task_source_key
                || assignment.task_id != lease.task_id
                || assignment.root_issue_id != lease.root_issue_id
                || assignment.workflow_run_id != lease.workflow_run_id
                || assignment.assignment_id != lease.assignment_id
                || !assignment.permitted_executors.contains(&lease.executor_id)
                || assignment.eligible
                || assignment.next_attempt == 0
                || lease.attempt != assignment.next_attempt
                || lease.completed
                || lease.route_selected
                || !lease.completion_eligible
                || lease_id != &make_lease_id(&lease.task_id, &lease.assignment_id, lease.attempt)
                || lease.slot_id != slot_id(&lease.executor_id, lease.slot_number)
                || !task.is_open
                || !task.workgraph_include
                || (lease.slot_number > agent.configured_slots
                    && !agent.retiring_slots.contains(&lease.slot_number))
                // A lease pins the actor metadata it was acquired with, which
                // is deliberately *not* re-checked against the live catalog: a
                // human renaming their GitHub account, or the catalog gaining
                // a next-generation node ID, must never wedge allocator sync
                // on an in-flight lease. Only the snapshot's own shape is
                // enforced; a new lease takes the new catalog.
                || !valid_lease_actor_snapshot(lease)
                // A recorded Dispatch identity is either absent, because the
                // lease predates the projector publishing one, or canonical.
                // An undispatched lease never carries one at all.
                || !valid_dispatch_identity(&lease.dispatch_id)
                || (!lease.has_dispatch && !lease.dispatch_id.is_empty())
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
        let mut selected_workgraph_tasks = BTreeSet::new();
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
                || lease.root_issue_id != task.root_issue_id
                || lease.workflow_run_id != task.workflow_run_id
                || lease.task_element_id != task.task_element_id
                || assignment.task_source_key != lease.task_source_key
                || assignment.task_id != lease.task_id
                || assignment.root_issue_id != lease.root_issue_id
                || assignment.workflow_run_id != lease.workflow_run_id
                || assignment.assignment_id != lease.assignment_id
                || !assignment.permitted_executors.contains(&lease.executor_id)
                || !lease.has_dispatch
                || lease.attempt == 0
                || lease.attempt > assignment.next_attempt
                || lease.lease_id
                    != make_lease_id(&lease.task_id, &lease.assignment_id, lease.attempt)
                || (lease.completed && !lease.completion_eligible)
                || (lease.route_selected
                    && (!lease.completion_eligible
                        || (task.is_open
                            && !self.workgraph_routes.values().any(|route| {
                                route.task_source_key == lease.task_source_key
                                    && route.attempt == lease.attempt
                                    && route.action != WORKGRAPH_ROUTE_REWORK
                            }))))
                || lease.slot_number == 0
                || lease.slot_id != slot_id(&lease.executor_id, lease.slot_number)
                || !valid_typed_workgraph_id(&lease.lease_id, "lease")
                || !valid_typed_workgraph_id(&lease.task_id, "task")
                || !valid_typed_workgraph_id(&lease.assignment_id, "assignment")
                || !valid_typed_workgraph_id(&lease.workflow_run_id, "workflow-run")
                || !valid_workgraph_id(&lease.executor_id)
                || !valid_workgraph_id(&lease.slot_id)
                || self
                    .workgraph_active
                    .get(&lease.lease_id)
                    .is_some_and(|active| active != lease)
                || ((lease.completed || lease.route_selected)
                    && !selected_workgraph_tasks.insert(&lease.task_source_key))
                || (lease.completed && task.is_open)
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
            .any(|lease| selected_workgraph_tasks.contains(&lease.task_source_key))
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
        if self.workgraph_routes.iter().any(|(source_key, route)| {
            source_key.trim().is_empty()
                || route.attempt == 0
                || route.attempt > MAX_WORKGRAPH_ATTEMPTS
                || route.max_attempts == 0
                || route.max_attempts > MAX_WORKGRAPH_ATTEMPTS
                || route.authorization_generation == 0
                || route.retry_slot_number == 0
                || !valid_workgraph_id(&route.retry_executor_id)
                || !self
                    .workgraph_authorizations
                    .get(&route.task_source_key)
                    .is_some_and(|authorization| {
                        authorization.included
                            && authorization.root_issue_id == route.root_issue_id
                            && authorization.generation == route.authorization_generation
                    })
                || !valid_workgraph_id(&route.task_source_key)
                || !valid_workgraph_id(&route.assignment_source_key)
                || !valid_typed_workgraph_id(&route.assignment_id, "assignment")
                || !valid_workgraph_id(&route.root_issue_id)
                || !valid_typed_workgraph_id(&route.workflow_run_id, "workflow-run")
                || !valid_typed_workgraph_id(&route.task_id, "task")
                || !valid_typed_workgraph_id(&route.result_id, "result")
                || !valid_typed_workgraph_id(&route.evaluation_id, "evaluation")
                || !valid_typed_workgraph_id(&route.route_id, "route")
                || !valid_workgraph_id(&route.action)
        }) {
            return Err("WorkGraph applied Route state is invalid".into());
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
        self.reconcile_workgraph_with_roots(
            projection,
            task_documents,
            &BTreeMap::new(),
            effective_from,
            now,
        )
    }

    fn reconcile_workgraph_with_roots(
        &mut self,
        projection: WorkGraphAllocatorProjection,
        task_documents: &BTreeMap<String, TaskDocument>,
        root_issues: &BTreeMap<String, RootIssueDocument>,
        effective_from: u64,
        now: DateTime<Utc>,
    ) -> AnyResult<AllocationDelta> {
        let mut delta = AllocationDelta::default();
        let active_at_start = self
            .workgraph_active
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let root_excluded = |root_issue_id: &str| {
            root_issues
                .get(root_issue_id)
                .is_some_and(|root| !root.workgraph_include)
        };
        let mut excluded_tasks = task_documents
            .iter()
            .filter(|(_, document)| !document.workgraph_include)
            .map(|(source_key, _)| source_key.clone())
            .collect::<BTreeSet<_>>();
        excluded_tasks.extend(
            projection
                .tasks
                .iter()
                .filter(|binding| root_excluded(&binding.root_issue_id))
                .map(|binding| binding.source_key.clone()),
        );
        excluded_tasks.extend(
            self.workgraph_task_identities
                .iter()
                .filter(|(_, task)| root_excluded(&task.root_issue_id))
                .map(|(source_key, _)| source_key.clone()),
        );
        let routed_tasks = projection
            .routes
            .iter()
            .map(|route| route.task_source_key.clone())
            .chain(
                self.workgraph_routes
                    .values()
                    .filter(|route| route.action != WORKGRAPH_ROUTE_REWORK)
                    .map(|route| route.task_source_key.clone()),
            )
            .collect::<BTreeSet<_>>();
        let newly_stale = self
            .workgraph_assignments
            .iter()
            .filter(|(_, assignment)| excluded_tasks.contains(assignment.task_source_key.as_str()))
            .map(|(source_key, _)| source_key.clone())
            .chain(
                self.workgraph_dispatched
                    .iter()
                    .filter_map(|(source_key, lease)| {
                        excluded_tasks
                            .contains(lease.task_source_key.as_str())
                            .then(|| source_key.clone())
                    }),
            )
            .chain(
                projection
                    .assignments
                    .iter()
                    .filter(|binding| excluded_tasks.contains(binding.task_source_key.as_str()))
                    .map(|binding| binding.source_key.clone()),
            )
            .chain(
                projection
                    .dispatches
                    .iter()
                    .filter(|binding| excluded_tasks.contains(binding.task_source_key.as_str()))
                    .map(|binding| binding.source_key.clone()),
            )
            .collect::<Vec<_>>();
        self.workgraph_stale_authorizations.extend(newly_stale);
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
            let workgraph_include =
                document.workgraph_include && !root_excluded(&binding.root_issue_id);
            let identity_changed = self
                .workgraph_task_identities
                .get(&binding.source_key)
                .is_some_and(|task| {
                    task.root_issue_id != binding.root_issue_id
                        || task.workflow_run_id != binding.workflow_run_id
                        || task.task_id != binding.task_id
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
                    root_issue_id: binding.root_issue_id.clone(),
                    workflow_run_id: binding.workflow_run_id.clone(),
                    task_id: binding.task_id.clone(),
                    task_element_id: binding.task_element_id.clone(),
                    is_open: document.is_open,
                    workgraph_include,
                },
            );
            if !document.is_open {
                self.deactivate_workgraph_task(&binding.source_key, &mut delta);
            } else if !workgraph_include {
                self.exclude_workgraph_task(&binding.source_key, &mut delta);
            }
        }

        let desired_assignments = projection
            .assignments
            .iter()
            .filter(|assignment| {
                !excluded_tasks.contains(assignment.task_source_key.as_str())
                    && !self
                        .workgraph_stale_authorizations
                        .contains(&assignment.source_key)
            })
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
            let preserve_history = self
                .workgraph_assignments
                .get(&source_key)
                .and_then(|assignment| {
                    self.workgraph_task_identities
                        .get(&assignment.task_source_key)
                })
                .is_some_and(|task| !task.workgraph_include);
            self.retract_workgraph_assignment(&source_key, preserve_history, &mut delta);
        }
        for assignment in projection.assignments.iter().filter(|assignment| {
            !excluded_tasks.contains(assignment.task_source_key.as_str())
                && !self
                    .workgraph_stale_authorizations
                    .contains(&assignment.source_key)
        }) {
            if let Some(state) = self.workgraph_assignments.get_mut(&assignment.source_key) {
                let task_open = self
                    .workgraph_task_identities
                    .get(&state.task_source_key)
                    .is_some_and(|task| task.is_open && task.workgraph_include);
                let owned = self
                    .workgraph_active
                    .values()
                    .any(|lease| lease.assignment_source_key == assignment.source_key);
                let eligible = task_open
                    && !owned
                    && !routed_tasks.contains(assignment.task_source_key.as_str());
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
                    root_issue_id: assignment.root_issue_id.clone(),
                    workflow_run_id: assignment.workflow_run_id.clone(),
                    task_id: assignment.task_id.clone(),
                    assignment_id: assignment.assignment_id.clone(),
                    permitted_executors: assignment.permitted_executors.clone(),
                    queued_at: effective_from,
                    next_attempt,
                    max_attempts: MAX_WORKGRAPH_ATTEMPTS,
                    eligible: task.is_open
                        && task.workgraph_include
                        && !routed_tasks.contains(assignment.task_source_key.as_str()),
                },
            );
            delta
                .affected_agents
                .extend(assignment.permitted_executors.iter().cloned());
        }

        let desired_dispatches = projection
            .dispatches
            .iter()
            .filter(|dispatch| {
                !excluded_tasks.contains(dispatch.task_source_key.as_str())
                    && !self
                        .workgraph_stale_authorizations
                        .contains(&dispatch.source_key)
            })
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

        let mut backfill: Vec<(String, String)> = Vec::new();
        for dispatch in projection.dispatches.iter().filter(|dispatch| {
            !excluded_tasks.contains(dispatch.task_source_key.as_str())
                && !self
                    .workgraph_stale_authorizations
                    .contains(&dispatch.source_key)
        }) {
            if self.workgraph_dispatched.contains_key(&dispatch.source_key) {
                // The Dispatch is already recorded, but a projector that has
                // begun publishing canonical Dispatch identities must be able
                // to fill one in on a lease dispatched before that rollout.
                backfill.push((dispatch.source_key.clone(), dispatch.dispatch_id.clone()));
                continue;
            }
            let applied_route_replay = projection.routes.iter().any(|route| {
                self.workgraph_routes.contains_key(&route.source_key)
                    && projection.results.iter().any(|result| {
                        result.result_id == route.result_id && result.lease_id == dispatch.lease_id
                    })
            });
            if applied_route_replay {
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
            lease.dispatch_id = dispatch.dispatch_id.clone();
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
        for (dispatch_source_key, dispatch_id) in backfill {
            self.backfill_workgraph_dispatch_id(&dispatch_source_key, &dispatch_id, &mut delta);
        }
        self.refresh_workgraph_pending_results(&projection);
        self.refresh_workgraph_pending_requests(&projection);

        for route in &projection.routes {
            let result = projection
                .results
                .iter()
                .find(|result| result.result_id == route.result_id)
                .expect("Route Result chain was validated");
            self.apply_workgraph_route(
                route,
                &projection,
                active_at_start.contains(&result.lease_id),
                now,
                &mut delta,
            )?;
        }
        self.allocate_workgraph(now, &mut delta);
        Ok(delta)
    }

    fn apply_workgraph_route(
        &mut self,
        route: &WorkGraphRouteBinding,
        projection: &WorkGraphAllocatorProjection,
        lease_was_active: bool,
        now: DateTime<Utc>,
        delta: &mut AllocationDelta,
    ) -> AnyResult<()> {
        let authorization_generation = self
            .workgraph_authorizations
            .get(&route.task_source_key)
            .filter(|authorization| {
                authorization.included && authorization.root_issue_id == route.root_issue_id
            })
            .map(|authorization| authorization.generation)
            .context("WorkGraph Route has no current task authorization generation")?;
        let result = projection
            .results
            .iter()
            .find(|result| result.result_id == route.result_id)
            .expect("Route Result chain was validated");
        let dispatch = projection
            .dispatches
            .iter()
            .find(|dispatch| dispatch.lease_id == result.lease_id)
            .expect("Route Dispatch chain was validated");
        let retry_slot_number = self
            .workgraph_active
            .get(&dispatch.lease_id)
            .or_else(|| {
                self.workgraph_dispatched
                    .values()
                    .find(|lease| lease.lease_id == dispatch.lease_id)
            })
            .map(|lease| lease.slot_number)
            .or_else(|| {
                self.workgraph_routes
                    .get(&route.source_key)
                    .map(|applied| applied.retry_slot_number)
            })
            .unwrap_or_default();
        let assignment_source_key = self
            .workgraph_assignments
            .iter()
            .find(|(_, assignment)| assignment.assignment_id == dispatch.assignment_id)
            .map(|(source_key, _)| source_key.clone())
            .context("WorkGraph Route Dispatch has no retained assignment")?;
        let state = WorkGraphRouteState {
            task_source_key: route.task_source_key.clone(),
            assignment_source_key,
            assignment_id: dispatch.assignment_id.clone(),
            root_issue_id: route.root_issue_id.clone(),
            workflow_run_id: route.workflow_run_id.clone(),
            task_id: route.task_id.clone(),
            result_id: route.result_id.clone(),
            evaluation_id: route.evaluation_id.clone(),
            route_id: route.route_id.clone(),
            action: route.action.clone(),
            attempt: route.attempt,
            max_attempts: route.max_attempts,
            authorization_generation,
            retry_executor_id: dispatch.executor_id.clone(),
            retry_slot_number,
        };
        if self
            .workgraph_routes
            .get(&route.source_key)
            .is_some_and(|applied| applied == &state)
        {
            if route.action == WORKGRAPH_ROUTE_REWORK {
                self.restore_workgraph_rework(&state, now, delta);
            } else if let Some(lease) = self
                .workgraph_dispatched
                .get_mut(&dispatch.source_key)
                .filter(|lease| {
                    lease.task_source_key == route.task_source_key
                        && lease.attempt == route.attempt
                        && !lease.completed
                        && !lease.route_selected
                })
            {
                lease.route_selected = true;
                delta.workgraph_historical.push(lease.clone());
            }
            return Ok(());
        }
        let Some(active) = self
            .workgraph_active
            .get(&dispatch.lease_id)
            .filter(|lease| {
                lease.has_dispatch
                    && lease.task_source_key == route.task_source_key
                    && lease.task_id == route.task_id
                    && lease.attempt == route.attempt
            })
            .cloned()
        else {
            // A late rework decision is retained and may restore queue
            // eligibility, but it can never release a newer selected lease.
            if route.action == WORKGRAPH_ROUTE_REWORK
                && !self
                    .workgraph_routes
                    .get(&route.source_key)
                    .is_some_and(|applied| applied.action != WORKGRAPH_ROUTE_REWORK)
            {
                self.workgraph_routes
                    .insert(route.source_key.clone(), state.clone());
                self.restore_workgraph_rework(&state, now, delta);
            }
            return Ok(());
        };

        if let Some(lease) = self.workgraph_active.get_mut(&active.lease_id) {
            lease.completion_eligible = route.action != WORKGRAPH_ROUTE_REWORK;
            lease.route_selected = route.action != WORKGRAPH_ROUTE_REWORK;
        }
        self.release_workgraph(&active.lease_id, false, delta);
        delta
            .workgraph_started
            .retain(|started| started.lease_id != active.lease_id);
        if !lease_was_active {
            delta
                .workgraph_released
                .retain(|released| released.lease_id != active.lease_id);
        }
        self.workgraph_routes
            .insert(route.source_key.clone(), state);

        let Some(assignment) = self
            .workgraph_assignments
            .get_mut(&active.assignment_source_key)
        else {
            return Ok(());
        };
        assignment.max_attempts = route.max_attempts;
        assignment.eligible = route.action == WORKGRAPH_ROUTE_REWORK;
        delta
            .affected_agents
            .extend(assignment.permitted_executors.iter().cloned());
        if route.action == WORKGRAPH_ROUTE_REWORK {
            self.allocate_workgraph_preferred(
                &active.assignment_source_key,
                &active.executor_id,
                active.slot_number,
                now,
                delta,
            );
        }
        Ok(())
    }

    fn restore_workgraph_rework(
        &mut self,
        route: &WorkGraphRouteState,
        now: DateTime<Utc>,
        delta: &mut AllocationDelta,
    ) {
        let current_generation = self
            .workgraph_authorizations
            .get(&route.task_source_key)
            .is_some_and(|authorization| {
                authorization.included
                    && authorization.root_issue_id == route.root_issue_id
                    && authorization.generation == route.authorization_generation
            });
        let actionable_task = self
            .workgraph_task_identities
            .get(&route.task_source_key)
            .is_some_and(|task| task.is_open && task.workgraph_include);
        if !current_generation
            || !actionable_task
            || self.task_has_terminal_route(&route.task_source_key)
        {
            return;
        }
        if self
            .workgraph_active
            .values()
            .any(|lease| lease.task_source_key == route.task_source_key)
        {
            return;
        }
        let assignment_source_key = route.assignment_source_key.clone();
        let Some(assignment) = self.workgraph_assignments.get_mut(&assignment_source_key) else {
            return;
        };
        if assignment.assignment_id != route.assignment_id
            || assignment.task_source_key != route.task_source_key
            || assignment.task_id != route.task_id
        {
            return;
        }
        assignment.max_attempts = route.max_attempts;
        assignment.eligible = assignment.next_attempt < assignment.max_attempts;
        delta
            .affected_agents
            .extend(assignment.permitted_executors.iter().cloned());
        self.allocate_workgraph_preferred(
            &assignment_source_key,
            &route.retry_executor_id,
            route.retry_slot_number,
            now,
            delta,
        );
    }

    fn retract_workgraph_assignment(
        &mut self,
        source_key: &str,
        preserve_history: bool,
        delta: &mut AllocationDelta,
    ) {
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
                if !preserve_history {
                    delta.workgraph_historical_ended.push(lease);
                }
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
            // The Dispatch identity is retracted with the Dispatch itself, so
            // a lease that is no longer dispatched can never keep binding a
            // worker response to a Dispatch that no longer exists.
            active.dispatch_id.clear();
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
            let terminal_route = self.task_has_terminal_route(&lease.task_source_key);
            if let Some(assignment) = self
                .workgraph_assignments
                .get_mut(&lease.assignment_source_key)
            {
                let eligible = self
                    .workgraph_task_identities
                    .get(&assignment.task_source_key)
                    .is_some_and(|task| task.is_open && task.workgraph_include)
                    && !terminal_route;
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
            self.retract_workgraph_assignment(&assignment_source, false, delta);
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
            self.workgraph_routes
                .retain(|_, route| route.task_source_key != source_key);
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

    fn exclude_workgraph_task(&mut self, source_key: &str, delta: &mut AllocationDelta) {
        self.workgraph_routes
            .retain(|_, route| route.task_source_key != source_key);
        for lease in self
            .workgraph_dispatched
            .values_mut()
            .filter(|lease| lease.task_source_key == source_key && lease.route_selected)
        {
            lease.route_selected = false;
            delta.workgraph_historical.push(lease.clone());
        }
        let leases = self
            .workgraph_active
            .values()
            .filter(|lease| lease.task_source_key == source_key)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in leases {
            self.cancel_workgraph_lease(&lease_id, delta);
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
            lease.route_selected = false;
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
                        && assignment.next_attempt < assignment.max_attempts
                        && assignment.permitted_executors.contains(&executor_id)
                        && !self
                            .workgraph_active
                            .values()
                            .any(|lease| lease.task_id == assignment.task_id)
                        && self
                            .workgraph_task_identities
                            .get(&assignment.task_source_key)
                            .is_some_and(|task| {
                                task.is_open
                                    && task.workgraph_include
                                    && task.task_id == assignment.task_id
                            })
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
                actor_kind: agent.kind,
                actor_github: agent.github.clone(),
                task_source_key: assignment.task_source_key.clone(),
                root_issue_id: assignment.root_issue_id.clone(),
                workflow_run_id: assignment.workflow_run_id.clone(),
                task_id: assignment.task_id.clone(),
                task_element_id: task.task_element_id.clone(),
                assignment_source_key,
                assignment_id,
                executor_id: executor_id.clone(),
                slot_id: slot,
                slot_number,
                attempt: assignment.next_attempt,
                acquired_at: timestamp(now),
                expires_at: timestamp(
                    now + chrono::Duration::seconds(agent.lease_duration_seconds),
                ),
                has_dispatch: false,
                dispatch_id: String::new(),
                completed: false,
                completion_eligible: true,
                route_selected: false,
            };
            self.workgraph_active.insert(lease_id, lease.clone());
            delta.workgraph_started.push(lease);
            delta.affected_agents.extend(affected_executors);
        }
    }

    fn task_has_terminal_route(&self, task_source_key: &str) -> bool {
        self.workgraph_routes.values().any(|route| {
            route.task_source_key == task_source_key && route.action != WORKGRAPH_ROUTE_REWORK
        })
    }

    fn allocate_workgraph_preferred(
        &mut self,
        assignment_source_key: &str,
        executor_id: &str,
        slot_number: u32,
        now: DateTime<Utc>,
        delta: &mut AllocationDelta,
    ) {
        let slot = slot_id(executor_id, slot_number);
        let available = self.agents.get(executor_id).is_some_and(|agent| {
            agent.configured && slot_number > 0 && slot_number <= agent.configured_slots
        }) && !self
            .workgraph_active
            .values()
            .any(|lease| lease.slot_id == slot);
        let eligible = self
            .workgraph_assignments
            .get(assignment_source_key)
            .is_some_and(|assignment| {
                assignment.eligible
                    && assignment.next_attempt < assignment.max_attempts
                    && assignment
                        .permitted_executors
                        .iter()
                        .any(|id| id == executor_id)
                    && self
                        .workgraph_task_identities
                        .get(&assignment.task_source_key)
                        .is_some_and(|task| task.is_open && task.workgraph_include)
            });
        if !available || !eligible {
            return;
        }

        let assignment = self
            .workgraph_assignments
            .get_mut(assignment_source_key)
            .expect("eligible preferred assignment exists");
        let attempt = self
            .workgraph_assignment_attempts
            .entry(assignment.assignment_id.clone())
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
            .expect("preferred assignment task exists");
        let agent = &self.agents[executor_id];
        let lease = WorkGraphActiveLease {
            lease_id: lease_id.clone(),
            actor_kind: agent.kind,
            actor_github: agent.github.clone(),
            task_source_key: assignment.task_source_key.clone(),
            root_issue_id: assignment.root_issue_id.clone(),
            workflow_run_id: assignment.workflow_run_id.clone(),
            task_id: assignment.task_id.clone(),
            task_element_id: task.task_element_id.clone(),
            assignment_source_key: assignment_source_key.to_string(),
            assignment_id: assignment.assignment_id.clone(),
            executor_id: executor_id.to_string(),
            slot_id: slot,
            slot_number,
            attempt: assignment.next_attempt,
            acquired_at: timestamp(now),
            expires_at: timestamp(now + chrono::Duration::seconds(agent.lease_duration_seconds)),
            has_dispatch: false,
            dispatch_id: String::new(),
            completed: false,
            completion_eligible: true,
            route_selected: false,
        };
        self.workgraph_active.insert(lease_id, lease.clone());
        delta.workgraph_started.push(lease);
        delta
            .affected_agents
            .extend(assignment.permitted_executors.iter().cloned());
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

    fn cancel_workgraph_lease(&mut self, lease_id: &str, delta: &mut AllocationDelta) {
        let Some(mut lease) = self.workgraph_active.remove(lease_id) else {
            return;
        };
        self.workgraph_result_claims.remove(lease_id);
        lease.completed = false;
        delta.affected_agents.insert(lease.executor_id.clone());
        if let Some(dispatched) = self
            .workgraph_dispatched
            .values_mut()
            .find(|dispatched| dispatched.lease_id == lease.lease_id)
        {
            *dispatched = lease.clone();
        }
        delta.workgraph_released.push(lease.clone());
        delta.workgraph_historical.push(lease.clone());
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
                let terminal_route = self
                    .workgraph_active
                    .get(&lease_id)
                    .is_some_and(|lease| self.task_has_terminal_route(&lease.task_source_key));
                if let Some(assignment) = self.workgraph_assignments.get_mut(&assignment_source) {
                    assignment.eligible =
                        !terminal_route && assignment.next_attempt < assignment.max_attempts;
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

    /// Recomputes the open evaluator subject of every task.
    ///
    /// The projector rebuilds the whole desired graph, so this is wholesale: a
    /// Result no Evaluation names yet is the one an evaluator answers. Each is
    /// recorded with the executor that produced it, resolved through the
    /// Result's Dispatch and the lease snapshot behind it. A previously
    /// recorded producer is carried forward when the lease it came from is no
    /// longer retained, so expiry, release, and restart never erase who must
    /// not review this Result.
    /// Recomputes the open assigner subject of every task.
    ///
    /// A request is open until the Assignment that answers it appears, so this
    /// mirrors the pending-Result rule exactly: the projector republishes the
    /// whole desired set on every batch, and a request a decision-bound
    /// Assignment names is retired here. Nothing is allocated: the map only
    /// records which question is still waiting on which task.
    fn refresh_workgraph_pending_requests(&mut self, projection: &WorkGraphAllocatorProjection) {
        let answered = projection
            .assignments
            .iter()
            .filter_map(|assignment| assignment.request_id.as_deref())
            .collect::<BTreeSet<_>>();
        self.workgraph_pending_requests = projection
            .assignment_requests
            .iter()
            .filter(|request| !answered.contains(request.request_id.as_str()))
            .map(|request| {
                (
                    request.task_source_key.clone(),
                    WorkGraphPendingRequest {
                        source_key: request.source_key.clone(),
                        request_id: request.request_id.clone(),
                        assigner_id: request.assigner_id.clone(),
                        candidates: request.candidates.clone(),
                    },
                )
            })
            .collect();
    }

    fn refresh_workgraph_pending_results(&mut self, projection: &WorkGraphAllocatorProjection) {
        let evaluated = projection
            .evaluations
            .iter()
            .map(|evaluation| evaluation.result_id.as_str())
            .collect::<BTreeSet<_>>();
        let previous = std::mem::take(&mut self.workgraph_pending_results)
            .into_values()
            .map(|pending| (pending.result_id.clone(), pending))
            .collect::<BTreeMap<_, _>>();
        self.workgraph_pending_results = projection
            .results
            .iter()
            .filter(|result| !evaluated.contains(result.result_id.as_str()))
            .map(|result| {
                let recorded = previous.get(&result.result_id);
                let executor_id = projection
                    .dispatches
                    .iter()
                    .find(|dispatch| dispatch.lease_id == result.lease_id)
                    .map(|dispatch| dispatch.executor_id.clone())
                    .or_else(|| recorded.map(|pending| pending.executor_id.clone()))
                    .unwrap_or_default();
                let producer_github = self
                    .workgraph_active
                    .get(&result.lease_id)
                    .or_else(|| {
                        self.workgraph_dispatched
                            .values()
                            .find(|lease| lease.lease_id == result.lease_id)
                    })
                    .and_then(|lease| lease.actor_github.clone())
                    .or_else(|| recorded.and_then(|pending| pending.producer_github.clone()))
                    .or_else(|| {
                        self.agents
                            .get(&executor_id)
                            .and_then(|agent| agent.github.clone())
                    });
                (
                    result.task_source_key.clone(),
                    WorkGraphPendingResult {
                        result_id: result.result_id.clone(),
                        lease_id: result.lease_id.clone(),
                        executor_id,
                        producer_github,
                    },
                )
            })
            .collect();
    }

    /// Records a canonical Dispatch identity on an already-dispatched lease.
    ///
    /// Lease timing is untouched: this only fills in an identity the projector
    /// did not previously publish, on both the dispatch-keyed and lease-keyed
    /// views, and republishes the lease so its projected node carries it. An
    /// empty projected identity never clears a recorded one, so a projector
    /// rollback cannot erase what was already observed.
    fn backfill_workgraph_dispatch_id(
        &mut self,
        dispatch_source_key: &str,
        dispatch_id: &str,
        delta: &mut AllocationDelta,
    ) {
        if dispatch_id.is_empty() || !valid_dispatch_identity(dispatch_id) {
            return;
        }
        let Some(lease) = self
            .workgraph_dispatched
            .get_mut(dispatch_source_key)
            .filter(|lease| lease.dispatch_id != dispatch_id)
        else {
            return;
        };
        lease.dispatch_id = dispatch_id.to_string();
        let lease = lease.clone();
        if let Some(active) = self.workgraph_active.get_mut(&lease.lease_id) {
            active.dispatch_id = lease.dispatch_id.clone();
        }
        if let Some(started) = delta
            .workgraph_started
            .iter_mut()
            .find(|started| started.lease_id == lease.lease_id)
        {
            *started = lease;
        } else if self.workgraph_active.contains_key(&lease.lease_id) {
            delta.workgraph_started.push(lease);
        } else if let Some(historical) = delta
            .workgraph_historical
            .iter_mut()
            .find(|historical| historical.lease_id == lease.lease_id)
        {
            *historical = lease;
        } else {
            delta.workgraph_historical.push(lease);
        }
    }

    pub fn sync_agents(&mut self, file: &AgentFile, now: DateTime<Utc>) -> AllocationDelta {
        let old: BTreeMap<_, _> = self
            .agents
            .iter()
            .map(|(id, agent)| (id.clone(), agent.slots()))
            .collect();
        let configured: BTreeSet<_> = file
            .actors
            .iter()
            .map(|actor| actor.actor_id.clone())
            .collect();
        for definition in &file.actors {
            let active = self.active_slots(&definition.actor_id);
            let mut agent = AgentState::new(definition);
            agent.retiring_slots = active
                .into_iter()
                .filter(|slot| *slot > definition.slots)
                .collect();
            self.agents.insert(definition.actor_id.clone(), agent);
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
                            && assignment.next_attempt < assignment.max_attempts
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
    derive_workgraph_id("lease", &[task, assignment, &attempt.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        PreparedProjection, PreparedProjectionCommit, WorkGraphEvaluateBinding,
        WorkGraphResultBinding, WorkGraphRouteBinding, WorkGraphTaskBinding,
    };
    use async_trait::async_trait;
    use chrono::TimeZone;
    use drasi_lib::wal::WriteAheadLogConfig;
    use drasi_lib::MemoryStateStoreProvider;
    use drasi_wal_redb::RedbWalProvider;
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    const TEST_TASK_ID: &str =
        "urn:drasi:workgraph:id:v1:task:sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const TEST_TASK_2_ID: &str =
        "urn:drasi:workgraph:id:v1:task:sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn test_id(id_type: &str, seed: &str) -> String {
        derive_workgraph_id(id_type, &[seed])
    }

    #[test]
    fn workgraph_typed_ids_require_exact_type_and_canonical_namespace() {
        assert!(valid_typed_workgraph_id(TEST_TASK_ID, "task"));
        assert!(!valid_typed_workgraph_id(TEST_TASK_ID, "assignment"));
        for invalid in [
            "task",
            "wgt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "workgraph-v1:task:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "urn:drasi:workgraph:id:v1:task:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "urn:drasi:workgraph:id:v1:task:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(!valid_typed_workgraph_id(invalid, "task"));
        }
    }

    #[test]
    fn lease_id_uses_canonical_cross_vector_and_decimal_attempt() {
        assert_eq!(
            make_lease_id("task-α", "assignment-β", 42),
            "urn:drasi:workgraph:id:v1:lease:sha256:\
             fdb83caf2b77a5b61b70c62d9cdb3111d7a2dbe10b57a99f3a5997634ee81a68"
        );
        assert_ne!(
            make_lease_id("task-α", "assignment-β", 42),
            make_lease_id("task-α", "assignment-β", 4)
        );
    }

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
                task_id: TEST_TASK_ID.to_string(),
                task_element_id: "task-element".to_string(),
                root_issue_id: "root".to_string(),
                workflow_run_id: test_id("workflow-run", "run"),
            }],
            assignment_requests: Vec::new(),
            assignments: vec![WorkGraphAssignmentBinding {
                source_key: "assignment-comment".to_string(),
                task_source_key: "issue".to_string(),
                task_id: TEST_TASK_ID.to_string(),
                assignment_id: test_id("assignment", "assignment"),
                permitted_executors: vec!["executor".to_string()],
                root_issue_id: "root".to_string(),
                workflow_run_id: test_id("workflow-run", "run"),
                request_id: None,
                response_id: None,
                assigner_id: None,
            }],
            dispatches,
            results: Vec::new(),
            evaluations: Vec::new(),
            routes: Vec::new(),
        }
    }

    fn task_document(workgraph_include: bool) -> TaskDocument {
        TaskDocument {
            assignees: Vec::new(),
            source_key: "issue".to_string(),
            body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
            is_open: true,
            state_reason: String::new(),
            parent_source_key: Some("root".to_string()),
            workgraph_labels: (!workgraph_include)
                .then(|| "workgraph:ignore".to_string())
                .into_iter()
                .collect(),
            workgraph_include,
        }
    }

    fn root_document(workgraph_include: bool) -> RootIssueDocument {
        RootIssueDocument {
            source_key: "root".to_string(),
            repository_owner: "acme".to_string(),
            repository_name: "widgets".to_string(),
            repository_node_id: "repository".to_string(),
            issue_number: 1,
            title: "Root".to_string(),
            body: "Root body".to_string(),
            is_open: true,
            admission_id: test_id("admission", "generation"),
            workflow_mappings: vec![crate::protocol::RootMappingAdmission {
                mapping_id: crate::protocol::LEGACY_WORKFLOW_MAPPING_ID.to_string(),
                label: crate::protocol::WORKGRAPH_ADMISSION_LABEL.to_string(),
                admission_id: test_id("admission", "generation"),
                title: "Root".to_string(),
                body: "Root body".to_string(),
                definition_repository: "acme/widgets".to_string(),
                definition_ref: "main".to_string(),
                definition_path: ".github/workgraph/workflows/issue-lifecycle-v1.body".to_string(),
            }],
            workgraph_labels: (!workgraph_include)
                .then(|| "workgraph:ignore".to_string())
                .into_iter()
                .collect(),
            workgraph_include,
        }
    }

    fn dispatch_binding(
        lease: &WorkGraphActiveLease,
        source_key: &str,
    ) -> WorkGraphDispatchBinding {
        WorkGraphDispatchBinding {
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
            source_key: source_key.to_string(),
            task_source_key: lease.task_source_key.clone(),
            root_issue_id: lease.root_issue_id.clone(),
            workflow_run_id: lease.workflow_run_id.clone(),
            task_id: lease.task_id.clone(),
            assignment_id: lease.assignment_id.clone(),
            lease_id: lease.lease_id.clone(),
            executor_id: lease.executor_id.clone(),
            slot_id: lease.slot_id.clone(),
        }
    }

    fn projection_with_route(
        lease: &WorkGraphActiveLease,
        dispatch_source: &str,
        route_source: &str,
        verdict: &str,
        action: &str,
        max_attempts: u64,
    ) -> WorkGraphAllocatorProjection {
        let suffix = lease.attempt.to_string();
        let result_id = test_id("result", &format!("result-{suffix}"));
        let evaluation_id = test_id("evaluation", &format!("evaluation-{suffix}"));
        let mut desired = projection(vec![dispatch_binding(lease, dispatch_source)]);
        desired.results.push(WorkGraphResultBinding {
            source_key: format!("result-comment-{suffix}"),
            task_source_key: lease.task_source_key.clone(),
            root_issue_id: lease.root_issue_id.clone(),
            workflow_run_id: lease.workflow_run_id.clone(),
            task_id: lease.task_id.clone(),
            result_id: result_id.clone(),
            lease_id: lease.lease_id.clone(),
            attempt: lease.attempt,
        });
        desired.evaluations.push(WorkGraphEvaluateBinding {
            source_key: format!("evaluation-comment-{suffix}"),
            task_source_key: lease.task_source_key.clone(),
            root_issue_id: lease.root_issue_id.clone(),
            workflow_run_id: lease.workflow_run_id.clone(),
            task_id: lease.task_id.clone(),
            result_id: result_id.clone(),
            evaluation_id: evaluation_id.clone(),
            attempt: lease.attempt,
            verdict: verdict.to_string(),
        });
        desired.routes.push(WorkGraphRouteBinding {
            source_key: route_source.to_string(),
            task_source_key: lease.task_source_key.clone(),
            root_issue_id: lease.root_issue_id.clone(),
            workflow_run_id: lease.workflow_run_id.clone(),
            task_id: lease.task_id.clone(),
            result_id,
            evaluation_id,
            route_id: test_id("route", &format!("route-{suffix}")),
            action: action.to_string(),
            attempt: lease.attempt,
            max_attempts,
        });
        desired
    }

    fn lifecycle_artifacts_for_projection(
        projection: &WorkGraphAllocatorProjection,
    ) -> BTreeMap<String, LifecycleArtifactDocument> {
        projection
            .assignment_requests
            .iter()
            .map(|binding| {
                (
                    &binding.source_key,
                    &binding.task_source_key,
                    WORKGRAPH_ASSIGNMENT_REQUEST_MARKER,
                )
            })
            .chain(projection.assignments.iter().map(|binding| {
                (
                    &binding.source_key,
                    &binding.task_source_key,
                    WORKGRAPH_ASSIGNMENT_MARKER,
                )
            }))
            .chain(projection.dispatches.iter().map(|binding| {
                (
                    &binding.source_key,
                    &binding.task_source_key,
                    WORKGRAPH_DISPATCH_MARKER,
                )
            }))
            .chain(projection.results.iter().map(|binding| {
                (
                    &binding.source_key,
                    &binding.task_source_key,
                    WORKGRAPH_RESULT_MARKER,
                )
            }))
            .chain(projection.evaluations.iter().map(|binding| {
                (
                    &binding.source_key,
                    &binding.task_source_key,
                    WORKGRAPH_EVALUATION_MARKER,
                )
            }))
            .chain(projection.routes.iter().map(|binding| {
                (
                    &binding.source_key,
                    &binding.task_source_key,
                    WORKGRAPH_ROUTE_MARKER,
                )
            }))
            .map(|(source_key, task_source_key, marker)| {
                (
                    source_key.clone(),
                    LifecycleArtifactDocument {
                        source_key: source_key.clone(),
                        task_source_key: task_source_key.clone(),
                        body: marker.to_string(),
                        created_at_revision: 1,
                        updated_at_revision: 1,
                    },
                )
            })
            .collect()
    }

    fn refresh_artifact_revisions(state: &mut AllocationState) {
        state.workgraph_artifact_revisions = state
            .workgraph_artifacts
            .iter()
            .map(|(source_key, artifact)| (source_key.clone(), artifact.updated_at_revision))
            .collect();
    }

    // ── First-class assigner: the AssignmentRequest action ────────────────

    const TEST_ASSIGNER_ID: &str = "triage-lead";

    fn test_request_id(seed: &str) -> String {
        test_id("assignment-request", seed)
    }

    /// A projection carrying one open AssignmentRequest and nothing else: the
    /// task has been asked who should do the work, and no answer exists yet.
    fn request_projection(candidates: Vec<String>) -> WorkGraphAllocatorProjection {
        let mut desired = projection(Vec::new());
        desired.assignments.clear();
        desired.assignment_requests = vec![WorkGraphAssignmentRequestBinding {
            source_key: "request-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            request_id: test_request_id("request"),
            assigner_id: TEST_ASSIGNER_ID.to_string(),
            candidates,
        }];
        desired
    }

    /// The Assignment that answers a request: it narrows the task to the one
    /// executor the assigner chose and names the whole decision.
    fn decision_assignment(selected: &str) -> WorkGraphAssignmentBinding {
        WorkGraphAssignmentBinding {
            source_key: "assignment-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            permitted_executors: vec![selected.to_string()],
            request_id: Some(test_request_id("request")),
            response_id: Some(test_id("response", "response")),
            assigner_id: Some(TEST_ASSIGNER_ID.to_string()),
        }
    }

    /// An open request is a question, never authority: it opens the assigner
    /// subject and touches no queue, slot, lease, or attempt.
    #[test]
    fn an_assignment_request_opens_a_subject_and_allocates_nothing() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);

        let delta = state
            .reconcile_workgraph(
                request_projection(vec!["executor".to_string()]),
                &documents,
                1,
                now,
            )
            .expect("reconcile the request");

        assert_eq!(
            state
                .workgraph_pending_requests
                .get("issue")
                .map(|pending| (pending.request_id.as_str(), pending.assigner_id.as_str())),
            Some((test_request_id("request").as_str(), TEST_ASSIGNER_ID))
        );
        // A request is lease-free. Nothing is queued, dispatched, held, or
        // counted against the executor's capacity.
        assert!(state.workgraph_assignments.is_empty());
        assert!(state.workgraph_assignment_attempts.is_empty());
        assert!(state.workgraph_active.is_empty());
        assert!(state.workgraph_dispatched.is_empty());
        assert!(delta.workgraph_started.is_empty());
        let runtime = state.agent_runtime();
        let executor = runtime.get("executor").expect("executor runtime");
        assert_eq!(executor.active_lease_count, 0);
        assert_eq!(executor.queue_depth, 0);
        assert_eq!(executor.available_slot_count, 1);
        state.validate().expect("state stays valid");
    }

    /// The decision retires the question, and only then does the work queue.
    #[test]
    fn the_decision_that_answers_a_request_retires_it_and_starts_the_work() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let candidates = vec!["executor".to_string(), "other-agent".to_string()];
        state
            .reconcile_workgraph(request_projection(candidates.clone()), &documents, 1, now)
            .expect("reconcile the request");
        assert!(state.workgraph_pending_requests.contains_key("issue"));

        let mut decided = request_projection(candidates);
        decided.assignments = vec![decision_assignment("executor")];
        state
            .reconcile_workgraph(decided.clone(), &documents, 2, now)
            .expect("reconcile the decision");

        // The answered request no longer holds a subject open, even though the
        // projector still republishes the request it answers.
        assert!(state.workgraph_pending_requests.is_empty());
        // Only now is the work queued, and only to the chosen executor.
        assert_eq!(
            state
                .workgraph_assignments
                .get("assignment-comment")
                .map(|assignment| assignment.permitted_executors.clone()),
            Some(vec!["executor".to_string()])
        );
        state.validate().expect("state stays valid");

        // A projector that stops publishing the request drops the subject too,
        // exactly as a retracted pending Result does.
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 3, now)
            .expect("reconcile without the request");
        assert!(state.workgraph_pending_requests.is_empty());
    }

    #[test]
    fn an_assignment_request_binding_is_proven_against_its_signed_artifact() {
        let valid = request_projection(vec!["executor".to_string(), "other-agent".to_string()]);
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let artifacts = lifecycle_artifacts_for_projection(&valid);
        validate_workgraph_projection(&valid, &documents, &artifacts).expect("valid request");

        // A request with no authenticated artifact is not evidence at all.
        assert!(
            validate_workgraph_projection(&valid, &documents, &BTreeMap::new()).is_err(),
            "an unsigned request must be refused"
        );

        // An artifact carrying some *other* marker cannot stand in for one.
        for marker in [
            WORKGRAPH_ASSIGNMENT_MARKER,
            WORKGRAPH_DISPATCH_MARKER,
            WORKGRAPH_RESULT_MARKER,
            "WorkGraphTaskAssignmentRequest/v2\n",
        ] {
            let mut spoofed = artifacts.clone();
            spoofed
                .get_mut("request-comment")
                .expect("request artifact")
                .body = marker.to_string();
            assert!(
                validate_workgraph_projection(&valid, &documents, &spoofed).is_err(),
                "{marker} must not pass as an AssignmentRequest"
            );
        }

        // Identity, shape, and separation of duties are all enforced.
        let mutations: [(&str, fn(&mut WorkGraphAssignmentRequestBinding)); 8] = [
            ("a non-canonical requestId", |request| {
                request.request_id = test_id("assignment", "request");
            }),
            ("a mismatched taskId", |request| {
                request.task_id = TEST_TASK_2_ID.to_string();
            }),
            ("a mismatched run", |request| {
                request.workflow_run_id = test_id("workflow-run", "other-run");
            }),
            ("an empty candidate set", |request| {
                request.candidates.clear();
            }),
            ("unsorted candidates", |request| {
                request.candidates = vec!["other-agent".to_string(), "executor".to_string()];
            }),
            ("duplicate candidates", |request| {
                request.candidates = vec!["executor".to_string(), "executor".to_string()];
            }),
            ("a self-dealing assigner", |request| {
                request.assigner_id = "executor".to_string();
            }),
            ("a blank assigner", |request| {
                request.assigner_id = String::new();
            }),
        ];
        for (reason, mutate) in mutations {
            let mut broken = valid.clone();
            mutate(&mut broken.assignment_requests[0]);
            assert!(
                validate_workgraph_projection(&broken, &documents, &artifacts).is_err(),
                "{reason} must be refused"
            );
        }

        // One task asks one question at a time.
        let mut twice = valid.clone();
        let mut second = twice.assignment_requests[0].clone();
        second.source_key = "request-comment-2".to_string();
        second.request_id = test_request_id("request-2");
        twice.assignment_requests.push(second);
        let mut both = artifacts.clone();
        both.insert(
            "request-comment-2".to_string(),
            LifecycleArtifactDocument {
                source_key: "request-comment-2".to_string(),
                task_source_key: "issue".to_string(),
                body: WORKGRAPH_ASSIGNMENT_REQUEST_MARKER.to_string(),
                created_at_revision: 1,
                updated_at_revision: 1,
            },
        );
        assert!(validate_workgraph_projection(&twice, &documents, &both).is_err());
    }

    #[test]
    fn an_assignment_decision_must_answer_a_request_it_was_actually_asked() {
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let candidates = vec!["executor".to_string(), "other-agent".to_string()];
        let mut valid = request_projection(candidates);
        valid.assignments = vec![decision_assignment("executor")];
        let artifacts = lifecycle_artifacts_for_projection(&valid);
        validate_workgraph_projection(&valid, &documents, &artifacts).expect("valid decision");

        // A legacy Assignment, which names nothing, is untouched.
        let mut legacy = valid.clone();
        legacy.assignment_requests.clear();
        legacy.assignments[0].request_id = None;
        legacy.assignments[0].response_id = None;
        legacy.assignments[0].assigner_id = None;
        legacy.assignments[0].permitted_executors =
            vec!["executor".to_string(), "other-agent".to_string()];
        let legacy_artifacts = lifecycle_artifacts_for_projection(&legacy);
        validate_workgraph_projection(&legacy, &documents, &legacy_artifacts)
            .expect("a legacy assignment still validates");

        // An agent assigner decides without a comment; the provenance is the
        // only optional part of a decision.
        let mut agent_decided = valid.clone();
        agent_decided.assignments[0].response_id = None;
        validate_workgraph_projection(&agent_decided, &documents, &artifacts)
            .expect("an agent assigner decides without a Response");

        let mutations: [(&str, fn(&mut WorkGraphAllocatorProjection)); 8] = [
            ("a decision naming no request", |desired| {
                desired.assignments[0].request_id = None;
            }),
            ("a decision naming no assigner", |desired| {
                desired.assignments[0].assigner_id = None;
            }),
            ("a dangling Response on a legacy body", |desired| {
                desired.assignments[0].request_id = None;
                desired.assignments[0].assigner_id = None;
            }),
            ("a request that was never asked", |desired| {
                desired.assignments[0].request_id = Some(test_request_id("never-asked"));
            }),
            ("an assigner the request never named", |desired| {
                desired.assignments[0].assigner_id = Some("release-lead".to_string());
            }),
            ("a decision that decided nothing", |desired| {
                desired.assignments[0].permitted_executors =
                    vec!["executor".to_string(), "other-agent".to_string()];
            }),
            ("an executor the request never offered", |desired| {
                desired.assignments[0].permitted_executors = vec!["stranger".to_string()];
            }),
            ("a non-canonical responseId", |desired| {
                desired.assignments[0].response_id = Some(test_id("result", "response"));
            }),
        ];
        for (reason, mutate) in mutations {
            let mut broken = valid.clone();
            mutate(&mut broken);
            assert!(
                validate_workgraph_projection(&broken, &documents, &artifacts).is_err(),
                "{reason} must be refused"
            );
        }

        // An assigner may never select itself out of its own candidate set.
        let mut self_dealing = valid.clone();
        self_dealing.assignment_requests[0]
            .candidates
            .push(TEST_ASSIGNER_ID.to_string());
        self_dealing.assignments[0].permitted_executors = vec![TEST_ASSIGNER_ID.to_string()];
        assert!(validate_workgraph_projection(&self_dealing, &documents, &artifacts).is_err());
    }

    #[test]
    fn assigner_response_state_invariants_are_proven_before_any_map_is_read() {
        let mut state = state_with_task_response();
        let source_key = "IC_response".to_string();
        let response = state
            .workgraph_task_responses
            .get_mut(&source_key)
            .expect("seeded response");
        response.role = TaskResponseRole::Assigner;
        response.dispatch_id = None;
        response.lease_id = None;
        response.request_id = Some(test_request_id("request"));
        state.workgraph_task_response_fingerprints.insert(
            source_key.clone(),
            task_response_fingerprint(&state.workgraph_task_responses[&source_key])
                .expect("fingerprint"),
        );
        state.validate().expect("an assigner response is coherent");

        // An assigner response that names no request, or names something that
        // is not a request, binds nothing.
        for broken in [
            None,
            Some(test_id("assignment", "request")),
            Some(test_id("response", "request")),
            Some(String::new()),
        ] {
            let mut invalid = state.clone();
            invalid
                .workgraph_task_responses
                .get_mut(&source_key)
                .expect("response")
                .request_id = broken;
            invalid.workgraph_task_response_fingerprints.insert(
                source_key.clone(),
                task_response_fingerprint(&invalid.workgraph_task_responses[&source_key])
                    .expect("fingerprint"),
            );
            assert!(invalid.validate().is_err());
        }

        // An assigner never reaches another role's subject, and no other role
        // may carry a request.
        for reach in [
            (|response: &mut TaskResponseDocument| {
                response.lease_id = Some(test_id("lease", "lease"));
            }) as fn(&mut TaskResponseDocument),
            |response| response.dispatch_id = Some(test_id("dispatch", "dispatch")),
            |response| response.result_id = Some(test_id("result", "result")),
            |response| response.role = TaskResponseRole::Evaluator,
        ] {
            let mut invalid = state.clone();
            reach(
                invalid
                    .workgraph_task_responses
                    .get_mut(&source_key)
                    .expect("response"),
            );
            invalid.workgraph_task_response_fingerprints.insert(
                source_key.clone(),
                task_response_fingerprint(&invalid.workgraph_task_responses[&source_key])
                    .expect("fingerprint"),
            );
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn a_pending_assignment_request_is_proven_before_it_opens_a_subject() {
        let mut state = AllocationState::default();
        let coherent = WorkGraphPendingRequest {
            source_key: "request-comment".to_string(),
            request_id: test_request_id("request"),
            assigner_id: TEST_ASSIGNER_ID.to_string(),
            candidates: vec!["executor".to_string(), "other-agent".to_string()],
        };
        state
            .workgraph_pending_requests
            .insert("issue".to_string(), coherent.clone());
        state.validate().expect("a coherent pending request");

        for mutate in [
            (|pending: &mut WorkGraphPendingRequest| {
                pending.request_id = test_id("assignment", "request");
            }) as fn(&mut WorkGraphPendingRequest),
            |pending| pending.assigner_id = String::new(),
            |pending| pending.source_key = String::new(),
            |pending| pending.candidates.clear(),
            |pending| pending.candidates = vec!["other-agent".to_string(), "executor".to_string()],
            |pending| pending.candidates = vec!["executor".to_string(), "executor".to_string()],
            |pending| pending.candidates.push(TEST_ASSIGNER_ID.to_string()),
        ] {
            let mut invalid = state.clone();
            let mut broken = coherent.clone();
            mutate(&mut broken);
            invalid
                .workgraph_pending_requests
                .insert("issue".to_string(), broken);
            assert!(invalid.validate().is_err());
        }
    }

    /// A checkpoint with no pending request is byte-identical to one written
    /// before first-class assigners existed, and both restore.
    #[test]
    fn an_assigner_free_checkpoint_round_trips_unchanged() {
        let state = AllocationState::default();
        let encoded = serde_json::to_string(&state).expect("encode state");
        assert!(!encoded.contains("workgraphPendingRequests"));
        let restored: AllocationState = serde_json::from_str(&encoded).expect("decode state");
        assert!(restored.workgraph_pending_requests.is_empty());
        restored.validate().expect("restored state is valid");
    }

    /// Excluding a task closes every question it had open. A request that
    /// outlived exclusion would keep the assigner subject open on work the
    /// graph no longer admits, so the fence retracts it with everything else.
    #[test]
    fn an_excluded_task_fences_its_open_assignment_request() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let desired = request_projection(vec!["executor".to_string(), "other-agent".to_string()]);
        authorize_task(&mut state, 1);
        state.workgraph_artifacts = lifecycle_artifacts_for_projection(&desired);
        refresh_artifact_revisions(&mut state);
        state.workgraph_artifact_generations = state
            .workgraph_artifacts
            .keys()
            .map(|source_key| (source_key.clone(), 1))
            .collect();

        // While the task is included the request is current, and the subject
        // it opens is real.
        let mut included = desired.clone();
        state.fence_stale_workgraph_authorizations(&mut included);
        assert_eq!(included.assignment_requests.len(), 1);
        state
            .reconcile_workgraph(included, &documents, 1, now)
            .expect("reconcile the included request");
        assert!(state.workgraph_pending_requests.contains_key("issue"));

        // Excluding the task retracts the question along with it.
        state
            .workgraph_authorizations
            .get_mut("issue")
            .expect("authorization")
            .included = false;
        let mut excluded = desired;
        state.fence_stale_workgraph_authorizations(&mut excluded);
        assert!(
            excluded.assignment_requests.is_empty(),
            "an excluded task keeps no open assignment request"
        );
        state
            .reconcile_workgraph(excluded, &documents, 2, now)
            .expect("reconcile the exclusion");
        assert!(
            state.workgraph_pending_requests.is_empty(),
            "an excluded task opens no assigner subject"
        );
        state.validate().expect("state stays valid");
    }

    /// A request written before an exclusion is not authority after the task is
    /// re-included: the new generation fences it. The decision that claimed to
    /// answer it falls with it, so no Assignment is ever left naming a request
    /// the projection no longer carries.
    #[test]
    fn a_stale_generation_request_fences_the_decision_that_answered_it() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut desired =
            request_projection(vec!["executor".to_string(), "other-agent".to_string()]);
        desired.assignments = vec![decision_assignment("executor")];
        let artifacts = lifecycle_artifacts_for_projection(&desired);
        validate_workgraph_projection(&desired, &documents, &artifacts)
            .expect("the pre-exclusion chain is coherent");

        let mut state = AllocationState::default();
        // The task was excluded and re-included, so the current generation is 2.
        // Only the request predates the exclusion; the decision is current.
        authorize_task(&mut state, 2);
        state.workgraph_artifacts = artifacts;
        refresh_artifact_revisions(&mut state);
        state.workgraph_artifact_generations = BTreeMap::from([
            ("request-comment".to_string(), 1),
            ("assignment-comment".to_string(), 2),
        ]);

        state.fence_stale_workgraph_authorizations(&mut desired);
        assert!(
            desired.assignment_requests.is_empty(),
            "a pre-exclusion request must not survive re-inclusion"
        );
        assert!(
            desired.assignments.is_empty(),
            "a decision never outlives the request it answered"
        );
        validate_workgraph_projection(
            &desired,
            &documents,
            &lifecycle_artifacts_for_projection(&desired),
        )
        .expect("the fenced projection is still coherent");

        state
            .reconcile_workgraph(desired, &documents, 1, now)
            .expect("reconcile the fenced projection");
        assert!(
            state.workgraph_pending_requests.is_empty(),
            "a stale request opens no assigner subject"
        );
        assert!(state.workgraph_assignments.is_empty());
        state.validate().expect("state stays valid");

        // Fencing a request never reaches an Assignment that answered no
        // request: legacy decisions survive on their own generation alone.
        let mut legacy = projection(Vec::new());
        state.workgraph_artifacts = lifecycle_artifacts_for_projection(&legacy);
        refresh_artifact_revisions(&mut state);
        state.workgraph_artifact_generations =
            BTreeMap::from([("assignment-comment".to_string(), 2)]);
        state.fence_stale_workgraph_authorizations(&mut legacy);
        assert_eq!(legacy.assignments.len(), 1);
    }

    fn authorize_task(state: &mut AllocationState, generation: u64) {
        state.workgraph_authorizations.insert(
            "issue".to_string(),
            WorkGraphAuthorizationState {
                root_issue_id: "root".to_string(),
                generation,
                cutoff_revision: 0,
                transition_revision: 0,
                included: true,
            },
        );
    }

    #[tokio::test]
    async fn issue_database_id_index_survives_allocator_recreation() {
        let mut state = AllocationState::default();
        state.workgraph_tasks.insert(
            "I_child".to_string(),
            TaskDocument {
                assignees: Vec::new(),
                source_key: "I_child".to_string(),
                body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                is_open: true,
                state_reason: String::new(),
                parent_source_key: Some("I_parent".to_string()),
                workgraph_labels: Vec::new(),
                workgraph_include: true,
            },
        );
        state
            .workgraph_issue_database_ids
            .insert(42, "I_child".to_string());
        state.validate().expect("database ID index is valid");

        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        store
            .set(
                "source",
                STATE_KEY,
                serde_json::to_vec(&state).expect("serialize state"),
            )
            .await
            .expect("persist state");
        let wal = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        wal.register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");

        let allocator = Allocator::new("source".to_string(), store, wal);

        assert_eq!(
            allocator
                .workgraph_issue_node_id(42)
                .await
                .expect("restore database ID index")
                .as_deref(),
            Some("I_child")
        );
    }

    fn task_response_document() -> TaskResponseDocument {
        let body = "@workgraph done".to_string();
        TaskResponseDocument {
            source_key: "IC_response".to_string(),
            task_source_key: "I_task".to_string(),
            actor_id: "human-agentofreality".to_string(),
            task_id: TEST_TASK_ID.to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            role: TaskResponseRole::Worker,
            dispatch_id: Some(test_id("dispatch", "dispatch")),
            lease_id: Some(test_id("lease", "lease")),
            request_id: None,
            result_id: None,
            author_database_id: 4_021_243,
            author_id: "MDQ6VXNlcjQwMjEyNDM=".to_string(),
            author_login: "agentofreality".to_string(),
            body_digest: derive_workgraph_response_body_digest(&body),
            body,
            created_at_revision: 10,
            updated_at_revision: 10,
        }
    }

    fn state_with_task_response() -> AllocationState {
        let mut state = AllocationState::default();
        let document = task_response_document();
        state
            .workgraph_task_response_revisions
            .insert(document.source_key.clone(), document.updated_at_revision);
        state.workgraph_task_response_fingerprints.insert(
            document.source_key.clone(),
            task_response_fingerprint(&document).expect("fingerprint"),
        );
        state
            .workgraph_task_responses
            .insert(document.source_key.clone(), document);
        state
    }

    #[test]
    fn the_response_revision_fingerprint_covers_github_evidence_only() {
        let recorded = task_response_document();
        let baseline = task_response_fingerprint(&recorded).expect("fingerprint");

        // The lifecycle subject and the actor it resolved to are allocator
        // derived and advance on their own. Folding them into the revision
        // fingerprint would make an identical redelivery look like
        // same-revision divergence every time the lifecycle moved.
        let mut rebound = recorded.clone();
        rebound.role = TaskResponseRole::Evaluator;
        rebound.dispatch_id = None;
        rebound.lease_id = None;
        rebound.result_id = Some(test_id("result", "result"));
        rebound.actor_id = "human-someone-else".to_string();
        assert_eq!(
            task_response_fingerprint(&rebound).expect("fingerprint"),
            baseline,
            "the subject must not participate in the revision fingerprint"
        );

        // Everything GitHub reported about the comment does participate.
        for mutate in [
            (|document: &mut TaskResponseDocument| {
                document.body = "@workgraph different".to_string();
                document.body_digest = derive_workgraph_response_body_digest(&document.body);
            }) as fn(&mut TaskResponseDocument),
            |document| document.author_database_id = 999,
            |document| document.author_login = "someone-else".to_string(),
            |document| document.author_id = "U_other".to_string(),
            |document| document.source_key = "IC_other".to_string(),
            |document| document.task_source_key = "I_other".to_string(),
            |document| document.created_at_revision = 9,
            |document| document.updated_at_revision = 11,
        ] {
            let mut diverged = recorded.clone();
            mutate(&mut diverged);
            assert_ne!(
                task_response_fingerprint(&diverged).expect("fingerprint"),
                baseline,
                "GitHub comment evidence must participate in the fingerprint"
            );
        }
    }

    #[test]
    fn a_pending_results_producer_survives_losing_every_trace_of_its_lease() {
        // A restart that pruned the lease, or a retracted dispatch record,
        // must not erase who produced a Result that is still judgeable.
        let lease_id = test_id("lease", "lease");
        let result_id = test_id("result", "result");
        let github = ActorGitHubIdentity {
            database_id: 4_021_243,
            node_id: "MDQ6VXNlcjQwMjEyNDM=".to_string(),
            login: "agentofreality".to_string(),
        };
        let mut state = AllocationState::default();
        state.workgraph_pending_results.insert(
            "issue".to_string(),
            WorkGraphPendingResult {
                result_id: result_id.clone(),
                lease_id: lease_id.clone(),
                executor_id: "human-agentofreality".to_string(),
                producer_github: Some(github.clone()),
            },
        );
        assert!(state.workgraph_active.is_empty());
        assert!(state.workgraph_dispatched.is_empty());

        let mut projection = projection(vec![WorkGraphDispatchBinding {
            dispatch_id: test_id("dispatch", "dispatch"),
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            lease_id: lease_id.clone(),
            executor_id: "human-agentofreality".to_string(),
            slot_id: "human-agentofreality/1".to_string(),
        }]);
        projection.assignments[0].permitted_executors = vec!["human-agentofreality".to_string()];
        projection.results = vec![WorkGraphResultBinding {
            source_key: "result-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            result_id: result_id.clone(),
            lease_id,
            attempt: 1,
        }];

        state.refresh_workgraph_pending_results(&projection);
        let pending = state
            .workgraph_pending_results
            .get("issue")
            .expect("the Result is still pending");
        assert_eq!(pending.result_id, result_id);
        assert_eq!(pending.executor_id, "human-agentofreality");
        assert_eq!(
            pending.producer_github.as_ref(),
            Some(&github),
            "the recorded producer must be carried forward"
        );
        // Which is exactly what keeps self review refused.
        assert!(pending.produced_by(4_021_243, "human-agentofreality"));
        assert!(pending.produced_by(4_021_243, "human-someone-else"));
        assert!(pending.produced_by(1, "human-agentofreality"));
        assert!(!pending.produced_by(5_150_001, "human-reviewer"));

        // An Evaluation retires the subject entirely.
        projection.evaluations = vec![WorkGraphEvaluateBinding {
            source_key: "evaluation-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            result_id,
            evaluation_id: test_id("evaluation", "evaluation"),
            attempt: 1,
            verdict: WORKGRAPH_EVALUATION_ACCEPTED.to_string(),
        }];
        state.refresh_workgraph_pending_results(&projection);
        assert!(state.workgraph_pending_results.is_empty());
    }

    #[test]
    fn task_response_state_invariants_are_proven_before_any_map_is_read() {
        state_with_task_response()
            .validate()
            .expect("a consistent task response state is valid");

        // A revision without its fingerprint.
        let mut orphaned = state_with_task_response();
        orphaned.workgraph_task_response_fingerprints.clear();
        assert!(orphaned.validate().is_err());

        // A document whose revision disagrees with its own updatedAtRevision.
        let mut skewed = state_with_task_response();
        skewed
            .workgraph_task_response_revisions
            .insert("IC_response".to_string(), 11);
        assert!(skewed.validate().is_err());

        // A document and a tombstone for the same comment.
        let mut both = state_with_task_response();
        both.workgraph_task_response_tombstones.insert(
            "IC_response".to_string(),
            TaskResponseIdentity {
                source_key: "IC_response".to_string(),
                task_source_key: "I_task".to_string(),
                task_id: TEST_TASK_ID.to_string(),
                actor_id: "human-agentofreality".to_string(),
            },
        );
        assert!(both.validate().is_err());

        // A tombstone with no revision behind it.
        let mut dangling = AllocationState::default();
        dangling.workgraph_task_response_tombstones.insert(
            "IC_response".to_string(),
            TaskResponseIdentity {
                source_key: "IC_response".to_string(),
                task_source_key: "I_task".to_string(),
                task_id: TEST_TASK_ID.to_string(),
                actor_id: "human-agentofreality".to_string(),
            },
        );
        assert!(dangling.validate().is_err());

        // A body whose digest does not bind it.
        let mut tampered = state_with_task_response();
        tampered
            .workgraph_task_responses
            .get_mut("IC_response")
            .expect("response")
            .body = "@workgraph tampered".to_string();
        assert!(tampered.validate().is_err());

        // A worker document that also names a Result, and an evaluator that
        // names a lease: neither subject shape is coherent.
        let mut worker_with_result = state_with_task_response();
        worker_with_result
            .workgraph_task_responses
            .get_mut("IC_response")
            .expect("response")
            .result_id = Some(test_id("result", "result"));
        assert!(worker_with_result.validate().is_err());
        let mut evaluator_with_lease = state_with_task_response();
        evaluator_with_lease
            .workgraph_task_responses
            .get_mut("IC_response")
            .expect("response")
            .role = TaskResponseRole::Evaluator;
        assert!(evaluator_with_lease.validate().is_err());

        // A pending evaluator subject that is not canonical, or that records
        // no producing executor to refuse self review against.
        for pending in [
            WorkGraphPendingResult {
                result_id: "not-a-result".to_string(),
                lease_id: test_id("lease", "lease"),
                executor_id: "executor".to_string(),
                producer_github: None,
            },
            WorkGraphPendingResult {
                result_id: test_id("result", "result"),
                lease_id: test_id("lease", "lease"),
                executor_id: String::new(),
                producer_github: None,
            },
            WorkGraphPendingResult {
                result_id: test_id("result", "result"),
                lease_id: "not-a-lease".to_string(),
                executor_id: "executor".to_string(),
                producer_github: None,
            },
        ] {
            let mut bad_pending = state_with_task_response();
            bad_pending
                .workgraph_pending_results
                .insert("I_task".to_string(), pending);
            assert!(bad_pending.validate().is_err());
        }
    }

    #[tokio::test]
    async fn a_corrupted_task_response_checkpoint_fails_restore_instead_of_panicking() {
        // A state that escaped validation on the way in must still be read
        // without unchecked indexing: the revision lookup reports nothing
        // rather than panicking on a missing fingerprint.
        let mut corrupted = state_with_task_response();
        corrupted.workgraph_task_response_fingerprints.clear();
        assert!(corrupted.validate().is_err());

        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        store
            .set(
                "source",
                STATE_KEY,
                serde_json::to_vec(&corrupted).expect("serialize state"),
            )
            .await
            .expect("persist state");
        let wal = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        wal.register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");
        let allocator = Allocator::new("source".to_string(), store, wal);
        let error = allocator
            .latest_workgraph_task_response_revision("IC_response")
            .await
            .expect_err("a corrupt task response checkpoint must not be served");
        assert!(
            error
                .to_string()
                .contains("task response revision state is invalid"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_task_response_revision_lookup_never_indexes_an_unproven_map() {
        // The accessor is total even against a state that bypassed
        // validation: an unpaired revision reports nothing rather than
        // panicking on a missing fingerprint or identity.
        let mut unpaired = AllocationState::default();
        unpaired
            .workgraph_task_response_revisions
            .insert("IC_orphan".to_string(), 10);
        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        let wal = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        wal.register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");
        let allocator = Allocator::new("source".to_string(), store, wal);
        let mut state = unpaired.clone();
        state.validate().expect_err("unpaired revision is invalid");
        // Reading the maps directly proves the accessor's own totality.
        assert!(allocator
            .latest_workgraph_task_response_revision("IC_missing")
            .await
            .expect("empty state reads cleanly")
            .is_none());
    }

    #[tokio::test]
    async fn a_valid_task_response_checkpoint_round_trips() {
        let state = state_with_task_response();
        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        store
            .set(
                "source",
                STATE_KEY,
                serde_json::to_vec(&state).expect("serialize state"),
            )
            .await
            .expect("persist state");
        let wal = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        wal.register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");
        let allocator = Allocator::new("source".to_string(), store, wal);
        let restored = allocator
            .latest_workgraph_task_response_revision("IC_response")
            .await
            .expect("restore task response revision")
            .expect("recorded revision");
        assert_eq!(restored.revision, 10);
        assert!(!restored.tombstone);
        assert_eq!(restored.identity.actor_id, "human-agentofreality");
        assert_eq!(restored.document.expect("document").body, "@workgraph done");
    }

    #[tokio::test]
    async fn invalid_dispatch_replacement_durably_retracts_prior_authorization() {
        let now = Utc::now();
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let documents = BTreeMap::from([(
            "issue".to_string(),
            TaskDocument {
                assignees: Vec::new(),
                source_key: "issue".to_string(),
                body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                is_open: true,
                state_reason: String::new(),
                parent_source_key: None,
                workgraph_labels: Vec::new(),
                workgraph_include: true,
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
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            lease_id: lease.lease_id.clone(),
            executor_id: "executor".to_string(),
            slot_id: lease.slot_id.clone(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
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
                    body: format!("{WORKGRAPH_ASSIGNMENT_MARKER}\n"),
                    created_at_revision: 1,
                    updated_at_revision: 1,
                },
            ),
            (
                "dispatch-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "dispatch-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_DISPATCH_MARKER}\n"),
                    created_at_revision: 1,
                    updated_at_revision: 1,
                },
            ),
        ]);
        refresh_artifact_revisions(&mut state);
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
                dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
                lease_id: test_id("lease", "not-the-active-lease"),
                ..accepted_dispatch
            },
        };
        let replacement = LifecycleArtifactDocument {
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            body: format!("{WORKGRAPH_DISPATCH_MARKER}\nreplacement"),
            created_at_revision: 1,
            updated_at_revision: 2,
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
                TEST_TASK_ID,
                &lease.lease_id,
                &test_id("assignment", "assignment"),
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
            [ProjectionInput::DeleteLifecycleArtifact { source_key, .. }]
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
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let documents = BTreeMap::from([(
            "issue".to_string(),
            TaskDocument {
                assignees: Vec::new(),
                source_key: "issue".to_string(),
                body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                is_open: true,
                state_reason: String::new(),
                parent_source_key: None,
                workgraph_labels: Vec::new(),
                workgraph_include: true,
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
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
            source_key: "dispatch-comment-1".to_string(),
            task_source_key: "issue".to_string(),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            lease_id: first.lease_id.clone(),
            executor_id: "executor".to_string(),
            slot_id: first.slot_id.clone(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
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
                TEST_TASK_ID,
                &first.lease_id,
                &test_id("assignment", "assignment"),
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
                TEST_TASK_ID,
                &first.lease_id,
                &test_id("assignment", "assignment"),
                "executor",
                &first.slot_id,
                now + chrono::Duration::seconds(61),
            )
            .is_none());
        assert!(state
            .workgraph_active_exact(
                TEST_TASK_ID,
                &second.lease_id,
                &test_id("assignment", "assignment"),
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
                    dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
                    source_key: "dispatch-comment-1".to_string(),
                    task_source_key: "issue".to_string(),
                    task_id: TEST_TASK_ID.to_string(),
                    assignment_id: test_id("assignment", "assignment"),
                    lease_id: first.lease_id.clone(),
                    executor_id: "executor".to_string(),
                    slot_id: first.slot_id.clone(),
                    root_issue_id: "root".to_string(),
                    workflow_run_id: test_id("workflow-run", "run"),
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
    fn exclusion_cancels_active_lease_and_fences_stale_authorization_after_restart() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let included = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let included_roots = BTreeMap::from([("root".to_string(), root_document(true))]);
        let mut desired = projection(Vec::new());
        state
            .reconcile_workgraph_with_roots(desired.clone(), &included, &included_roots, 1, now)
            .expect("initial allocation");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("active lease");
        assert_eq!(first.root_issue_id, "root");
        assert_eq!(first.workflow_run_id, test_id("workflow-run", "run"));
        desired.dispatches.push(WorkGraphDispatchBinding {
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            lease_id: first.lease_id.clone(),
            executor_id: "executor".to_string(),
            slot_id: first.slot_id.clone(),
        });
        state
            .reconcile_workgraph_with_roots(
                desired.clone(),
                &included,
                &included_roots,
                2,
                now + chrono::Duration::seconds(1),
            )
            .expect("accept Dispatch");
        assert!(state
            .workgraph_active
            .get(&first.lease_id)
            .is_some_and(|lease| lease.has_dispatch));

        let excluded_roots = BTreeMap::from([("root".to_string(), root_document(false))]);
        let delta = state
            .reconcile_workgraph_with_roots(
                desired.clone(),
                &included,
                &excluded_roots,
                3,
                now + chrono::Duration::seconds(2),
            )
            .expect("exclude task");
        assert!(state.workgraph_active.is_empty());
        assert!(delta
            .workgraph_released
            .iter()
            .any(|lease| lease.lease_id == first.lease_id && !lease.completed));
        assert!(delta
            .workgraph_historical
            .iter()
            .any(|lease| lease.lease_id == first.lease_id && !lease.completed));
        assert!(delta.workgraph_ended.is_empty());
        assert!(delta.workgraph_historical_ended.is_empty());
        assert!(state
            .workgraph_stale_authorizations
            .contains("assignment-comment"));
        assert!(state
            .workgraph_stale_authorizations
            .contains("dispatch-comment"));
        assert!(state
            .workgraph_task_identities
            .get("issue")
            .is_some_and(|task| !task.workgraph_include));

        let bytes = serde_json::to_vec(&state).expect("serialize durable state");
        let mut restarted: AllocationState =
            serde_json::from_slice(&bytes).expect("restore durable state");
        restarted.validate().expect("restored state");
        restarted
            .reconcile_workgraph_with_roots(desired.clone(), &included, &included_roots, 4, now)
            .expect("re-include task");
        assert!(restarted.workgraph_active.is_empty());

        let mut fresh = desired.clone();
        fresh.assignments[0].source_key = "assignment-comment-fresh".to_string();
        fresh.assignments[0].assignment_id = "assignment-fresh".to_string();
        restarted
            .reconcile_workgraph_with_roots(fresh, &included, &included_roots, 5, now)
            .expect("fresh authorization");
        let fresh_lease = restarted
            .workgraph_active
            .values()
            .next()
            .expect("fresh lease");
        assert_ne!(fresh_lease.lease_id, desired.assignments[0].assignment_id);

        restarted
            .reconcile_workgraph_with_roots(desired, &included, &included_roots, 6, now)
            .expect("reordered stale authorization");
        assert!(restarted.workgraph_active.is_empty());
        restarted.validate().expect("final state");
    }

    #[test]
    fn authorization_generation_rejects_unseen_pre_exclusion_assignment_after_reinclude() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let tasks = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let included_roots = BTreeMap::from([("root".to_string(), root_document(true))]);
        let excluded_roots = BTreeMap::from([("root".to_string(), root_document(false))]);
        let mut task_only = projection(Vec::new());
        task_only.assignments.clear();
        let mut revisions = BTreeMap::from([("issue".to_string(), 100), ("root".to_string(), 100)]);
        state.refresh_workgraph_authorizations(
            &task_only,
            &tasks,
            &included_roots,
            &revisions,
            &BTreeMap::new(),
            &[],
        );
        state
            .reconcile_workgraph_with_roots(task_only.clone(), &tasks, &included_roots, 1, now)
            .expect("initial inclusion");
        assert_eq!(state.workgraph_authorizations["issue"].generation, 1);

        revisions.insert("root".to_string(), 200);
        state.refresh_workgraph_authorizations(
            &task_only,
            &tasks,
            &excluded_roots,
            &revisions,
            &BTreeMap::new(),
            &[],
        );
        state
            .reconcile_workgraph_with_roots(task_only.clone(), &tasks, &excluded_roots, 2, now)
            .expect("exclude root");
        assert_eq!(state.workgraph_authorizations["issue"].generation, 2);
        assert_eq!(state.workgraph_authorizations["issue"].cutoff_revision, 200);

        revisions.insert("root".to_string(), 300);
        state.refresh_workgraph_authorizations(
            &task_only,
            &tasks,
            &included_roots,
            &revisions,
            &BTreeMap::new(),
            &[],
        );
        state
            .reconcile_workgraph_with_roots(task_only, &tasks, &included_roots, 3, now)
            .expect("re-include root");
        assert_eq!(state.workgraph_authorizations["issue"].generation, 3);
        assert_eq!(state.workgraph_authorizations["issue"].cutoff_revision, 300);

        let bytes = serde_json::to_vec(&state).expect("serialize generation");
        let mut restarted: AllocationState =
            serde_json::from_slice(&bytes).expect("restore generation");
        let assignment_artifact = LifecycleArtifactDocument {
            source_key: "assignment-comment".to_string(),
            task_source_key: "issue".to_string(),
            body: format!("{WORKGRAPH_ASSIGNMENT_MARKER}\n"),
            created_at_revision: 150,
            updated_at_revision: 150,
        };
        let dispatch_artifact = LifecycleArtifactDocument {
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            body: format!("{WORKGRAPH_DISPATCH_MARKER}\n"),
            created_at_revision: 150,
            updated_at_revision: 150,
        };
        let artifacts = BTreeMap::from([
            (
                "assignment-comment".to_string(),
                assignment_artifact.clone(),
            ),
            ("dispatch-comment".to_string(), dispatch_artifact.clone()),
        ]);
        let mut late = projection(vec![WorkGraphDispatchBinding {
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            lease_id: test_id("lease", "unseen-old-lease"),
            executor_id: "executor".to_string(),
            slot_id: "executor:1".to_string(),
        }]);
        restarted.refresh_workgraph_authorizations(
            &late,
            &tasks,
            &included_roots,
            &revisions,
            &artifacts,
            &[
                ProjectionInput::UpsertLifecycleArtifact(assignment_artifact),
                ProjectionInput::UpsertLifecycleArtifact(dispatch_artifact),
            ],
        );
        restarted.fence_stale_workgraph_authorizations(&mut late);
        assert!(late.assignments.is_empty());
        assert!(late.dispatches.is_empty());
        assert_eq!(
            restarted.workgraph_artifact_generations["assignment-comment"],
            0
        );
        restarted
            .reconcile_workgraph_with_roots(late, &tasks, &included_roots, 4, now)
            .expect("ignore stale unseen assignment");
        assert!(restarted.workgraph_active.is_empty());
    }

    #[test]
    fn explicit_reinclude_fences_prior_generation_in_both_delivery_orders() {
        for exclusion_first in [true, false] {
            let mut state = AllocationState::default();
            let tasks = BTreeMap::from([("issue".to_string(), task_document(true))]);
            let included_roots = BTreeMap::from([("root".to_string(), root_document(true))]);
            let excluded_roots = BTreeMap::from([("root".to_string(), root_document(false))]);
            let mut task_only = projection(Vec::new());
            task_only.assignments.clear();
            let mut revisions =
                BTreeMap::from([("issue".to_string(), 100), ("root".to_string(), 100)]);
            state.refresh_workgraph_authorizations(
                &task_only,
                &tasks,
                &included_roots,
                &revisions,
                &BTreeMap::new(),
                &[],
            );

            if exclusion_first {
                revisions.insert("root".to_string(), 200);
                state.refresh_workgraph_authorizations(
                    &task_only,
                    &tasks,
                    &excluded_roots,
                    &revisions,
                    &BTreeMap::new(),
                    &[ProjectionInput::RecordIssueRevision {
                        source_key: "root".to_string(),
                        revision: 200,
                        state_fingerprint: "0".repeat(64),
                        authorization_transition: true,
                    }],
                );
            }
            revisions.insert("root".to_string(), 300);
            state.refresh_workgraph_authorizations(
                &task_only,
                &tasks,
                &included_roots,
                &revisions,
                &BTreeMap::new(),
                &[ProjectionInput::RecordIssueRevision {
                    source_key: "root".to_string(),
                    revision: 300,
                    state_fingerprint: "1".repeat(64),
                    authorization_transition: true,
                }],
            );

            let authorization = &state.workgraph_authorizations["issue"];
            assert!(authorization.included);
            assert_eq!(authorization.cutoff_revision, 300);
            assert_eq!(
                authorization.generation,
                if exclusion_first { 3 } else { 2 }
            );
            let old = LifecycleArtifactDocument {
                source_key: "old-assignment".to_string(),
                task_source_key: "issue".to_string(),
                body: format!("{WORKGRAPH_ASSIGNMENT_MARKER}\n"),
                created_at_revision: 150,
                updated_at_revision: 150,
            };
            let artifacts = BTreeMap::from([("old-assignment".to_string(), old.clone())]);
            let mut desired = projection(Vec::new());
            desired.assignments[0].source_key = "old-assignment".to_string();
            state.refresh_workgraph_authorizations(
                &desired,
                &tasks,
                &included_roots,
                &revisions,
                &artifacts,
                &[ProjectionInput::UpsertLifecycleArtifact(old)],
            );
            state.fence_stale_workgraph_authorizations(&mut desired);
            assert!(desired.assignments.is_empty());
        }
    }

    #[test]
    fn assignment_delivered_before_task_converges_when_authorization_arrives() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let artifact = LifecycleArtifactDocument {
            source_key: "assignment-comment".to_string(),
            task_source_key: "issue".to_string(),
            body: format!("{WORKGRAPH_ASSIGNMENT_MARKER}\n"),
            created_at_revision: 200,
            updated_at_revision: 200,
        };
        let artifacts = BTreeMap::from([("assignment-comment".to_string(), artifact.clone())]);
        state.refresh_workgraph_authorizations(
            &WorkGraphAllocatorProjection::default(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &artifacts,
            &[ProjectionInput::UpsertLifecycleArtifact(artifact)],
        );
        state.workgraph_artifacts = artifacts.clone();
        refresh_artifact_revisions(&mut state);
        assert_eq!(
            state.workgraph_artifact_generations["assignment-comment"],
            0
        );

        let tasks = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let roots = BTreeMap::from([("root".to_string(), root_document(true))]);
        let revisions = BTreeMap::from([("issue".to_string(), 100), ("root".to_string(), 100)]);
        let mut desired = projection(Vec::new());
        state.refresh_workgraph_authorizations(
            &desired,
            &tasks,
            &roots,
            &revisions,
            &artifacts,
            &[],
        );
        state.fence_stale_workgraph_authorizations(&mut desired);
        assert_eq!(desired.assignments.len(), 1);
        state
            .reconcile_workgraph_with_roots(desired, &tasks, &roots, 1, now)
            .expect("allocate retained assignment");
        assert_eq!(state.workgraph_active.len(), 1);
    }

    #[test]
    fn dispatch_delivered_before_task_converges_after_assignment_allocation() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let assignment = LifecycleArtifactDocument {
            source_key: "assignment-comment".to_string(),
            task_source_key: "issue".to_string(),
            body: format!("{WORKGRAPH_ASSIGNMENT_MARKER}\n"),
            created_at_revision: 200,
            updated_at_revision: 200,
        };
        let dispatch = LifecycleArtifactDocument {
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            body: format!("{WORKGRAPH_DISPATCH_MARKER}\n"),
            created_at_revision: 210,
            updated_at_revision: 210,
        };
        let artifacts = BTreeMap::from([
            ("assignment-comment".to_string(), assignment.clone()),
            ("dispatch-comment".to_string(), dispatch.clone()),
        ]);
        state.refresh_workgraph_authorizations(
            &WorkGraphAllocatorProjection::default(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &artifacts,
            &[
                ProjectionInput::UpsertLifecycleArtifact(assignment),
                ProjectionInput::UpsertLifecycleArtifact(dispatch),
            ],
        );
        state.workgraph_artifacts = artifacts.clone();
        refresh_artifact_revisions(&mut state);

        let tasks = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let roots = BTreeMap::from([("root".to_string(), root_document(true))]);
        let revisions = BTreeMap::from([("issue".to_string(), 100), ("root".to_string(), 100)]);
        let mut assignment_projection = projection(Vec::new());
        state.refresh_workgraph_authorizations(
            &assignment_projection,
            &tasks,
            &roots,
            &revisions,
            &artifacts,
            &[],
        );
        state.fence_stale_workgraph_authorizations(&mut assignment_projection);
        state
            .reconcile_workgraph_with_roots(assignment_projection, &tasks, &roots, 1, now)
            .expect("allocate retained assignment");
        let lease = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("active lease");
        let mut dispatch_projection = projection(vec![WorkGraphDispatchBinding {
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            lease_id: lease.lease_id.clone(),
            executor_id: "executor".to_string(),
            slot_id: lease.slot_id,
        }]);
        state.fence_stale_workgraph_authorizations(&mut dispatch_projection);
        assert_eq!(dispatch_projection.dispatches.len(), 1);
        state
            .reconcile_workgraph_with_roots(dispatch_projection, &tasks, &roots, 2, now)
            .expect("accept retained Dispatch");
        assert!(state
            .workgraph_active
            .get(&lease.lease_id)
            .is_some_and(|active| active.has_dispatch));
    }

    #[test]
    fn prior_allocator_schema_is_rejected_explicitly() {
        let mut encoded = serde_json::to_value(AllocationState::default()).expect("encode state");
        encoded["version"] = serde_json::json!(VERSION - 1);
        let state: AllocationState = serde_json::from_value(encoded).expect("decode old state");
        assert!(state
            .validate()
            .expect_err("a prior schema must be rejected")
            .contains(&format!("version must equal {VERSION}")));
    }

    fn comment_with_admissions(admission_ids: Vec<String>) -> RootIssueCommentDocument {
        let admission_id = admission_ids
            .first()
            .cloned()
            .unwrap_or_else(|| test_id("admission", "compat"));
        RootIssueCommentDocument {
            source_key: "IC_human".to_string(),
            root_issue_id: "I_root".to_string(),
            admission_id,
            admission_ids,
            repository_owner: "acme".to_string(),
            repository_name: "widgets".to_string(),
            repository_node_id: "R_widgets".to_string(),
            issue_number: 6,
            author_id: "U_human".to_string(),
            author_type: "User".to_string(),
            author_login: "octocat".to_string(),
            body: "resume".to_string(),
            created_at_revision: 1,
            updated_at_revision: 1,
        }
    }

    #[test]
    fn root_comment_admission_sets_must_be_ordered_typed_and_self_consistent() {
        let foo = test_id("admission", "foo");
        let bar = test_id("admission", "bar");
        let mut ordered = vec![foo.clone(), bar.clone()];
        ordered.sort();

        let valid = comment_with_admissions(ordered.clone());
        assert!(valid_root_comment_admission_ids(&valid));

        // Legacy documents carry no set at all.
        assert!(valid_root_comment_admission_ids(&comment_with_admissions(
            Vec::new()
        )));

        let mut unordered = valid.clone();
        unordered.admission_ids = ordered.iter().rev().cloned().collect();
        assert!(
            !valid_root_comment_admission_ids(&unordered),
            "an unordered set must be rejected"
        );

        let mut duplicated = valid.clone();
        duplicated.admission_ids = vec![foo.clone(), foo.clone()];
        duplicated.admission_id = foo.clone();
        assert!(
            !valid_root_comment_admission_ids(&duplicated),
            "a repeated admission must be rejected"
        );

        let mut untyped = valid.clone();
        untyped.admission_ids = vec!["not-a-workgraph-id".to_string()];
        untyped.admission_id = "not-a-workgraph-id".to_string();
        assert!(
            !valid_root_comment_admission_ids(&untyped),
            "every admission must be a typed WorkGraph ID"
        );

        let mut orphaned = valid.clone();
        orphaned.admission_id = test_id("admission", "elsewhere");
        assert!(
            !valid_root_comment_admission_ids(&orphaned),
            "the compatibility admission must be one of the set"
        );
    }

    #[test]
    fn all_action_bindings_validate_direct_root_run_and_task_identities() {
        let lease_id = make_lease_id(TEST_TASK_ID, &test_id("assignment", "assignment"), 1);
        let mut desired = projection(vec![WorkGraphDispatchBinding {
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
            source_key: "dispatch-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            assignment_id: test_id("assignment", "assignment"),
            lease_id: lease_id.clone(),
            executor_id: "executor".to_string(),
            slot_id: "executor:1".to_string(),
        }]);
        desired.results.push(WorkGraphResultBinding {
            source_key: "result-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            result_id: test_id("result", "result"),
            lease_id,
            attempt: 1,
        });
        desired.evaluations.push(WorkGraphEvaluateBinding {
            source_key: "evaluate-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            result_id: test_id("result", "result"),
            evaluation_id: test_id("evaluation", "evaluation"),
            attempt: 1,
            verdict: WORKGRAPH_EVALUATION_ACCEPTED.to_string(),
        });
        desired.routes.push(WorkGraphRouteBinding {
            source_key: "route-comment".to_string(),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            result_id: test_id("result", "result"),
            evaluation_id: test_id("evaluation", "evaluation"),
            route_id: test_id("route", "route"),
            action: "complete".to_string(),
            attempt: 1,
            max_attempts: 3,
        });
        let tasks = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let artifacts = BTreeMap::from([
            (
                "dispatch-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "dispatch-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_DISPATCH_MARKER}\n"),
                    created_at_revision: 1,
                    updated_at_revision: 1,
                },
            ),
            (
                "assignment-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "assignment-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_ASSIGNMENT_MARKER}\n"),
                    created_at_revision: 1,
                    updated_at_revision: 1,
                },
            ),
            (
                "result-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "result-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_RESULT_MARKER}\n"),
                    created_at_revision: 1,
                    updated_at_revision: 1,
                },
            ),
            (
                "evaluate-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "evaluate-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_EVALUATION_MARKER}\n"),
                    created_at_revision: 1,
                    updated_at_revision: 1,
                },
            ),
            (
                "route-comment".to_string(),
                LifecycleArtifactDocument {
                    source_key: "route-comment".to_string(),
                    task_source_key: "issue".to_string(),
                    body: format!("{WORKGRAPH_ROUTE_MARKER}\n"),
                    created_at_revision: 1,
                    updated_at_revision: 1,
                },
            ),
        ]);

        validate_workgraph_projection(&desired, &tasks, &artifacts)
            .expect("matching direct identities");
        let encoded = serde_json::to_value(&desired).expect("serialize projection");
        assert_eq!(encoded["results"][0]["rootIssueId"], "root");
        assert_eq!(
            encoded["evaluations"][0]["workflowRunId"],
            test_id("workflow-run", "run")
        );
        assert_eq!(
            encoded["routes"][0]["evaluationId"],
            test_id("evaluation", "evaluation")
        );

        desired.evaluations[0].workflow_run_id = "other-run".to_string();
        assert!(validate_workgraph_projection(&desired, &tasks, &artifacts).is_err());
    }

    #[test]
    fn every_known_projection_id_boundary_rejects_a_wrong_type() {
        let tasks = BTreeMap::from([("issue".to_string(), task_document(true))]);

        let mut task_wrong = projection(Vec::new());
        let wrong_task = test_id("assignment", "wrong-task");
        task_wrong.tasks[0].task_id = wrong_task.clone();
        task_wrong.assignments[0].task_id = wrong_task;
        assert!(validate_workgraph_projection(
            &task_wrong,
            &tasks,
            &lifecycle_artifacts_for_projection(&task_wrong)
        )
        .is_err());

        let mut run_wrong = projection(Vec::new());
        let wrong_run = test_id("route", "wrong-run");
        run_wrong.tasks[0].workflow_run_id = wrong_run.clone();
        run_wrong.assignments[0].workflow_run_id = wrong_run;
        assert!(validate_workgraph_projection(
            &run_wrong,
            &tasks,
            &lifecycle_artifacts_for_projection(&run_wrong)
        )
        .is_err());

        let mut assignment_wrong = projection(Vec::new());
        assignment_wrong.assignments[0].assignment_id = test_id("task", "wrong-assignment");
        assert!(validate_workgraph_projection(
            &assignment_wrong,
            &tasks,
            &lifecycle_artifacts_for_projection(&assignment_wrong)
        )
        .is_err());

        let assignment_id = test_id("assignment", "assignment");
        let lease = WorkGraphActiveLease {
            dispatch_id: String::new(),
            actor_kind: ActorKind::Agent,
            actor_github: None,
            lease_id: make_lease_id(TEST_TASK_ID, &assignment_id, 1),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            task_element_id: "task-element".to_string(),
            assignment_source_key: "assignment-comment".to_string(),
            assignment_id,
            executor_id: "executor".to_string(),
            slot_id: "executor:1".to_string(),
            slot_number: 1,
            attempt: 1,
            acquired_at: "2026-01-01T00:00:00.000Z".to_string(),
            expires_at: "2026-01-01T00:01:00.000Z".to_string(),
            has_dispatch: true,
            completed: false,
            completion_eligible: true,
            route_selected: false,
        };
        let complete = projection_with_route(
            &lease,
            "dispatch-comment",
            "route-comment",
            WORKGRAPH_EVALUATION_ACCEPTED,
            "complete",
            3,
        );
        let artifacts = lifecycle_artifacts_for_projection(&complete);
        validate_workgraph_projection(&complete, &tasks, &artifacts).expect("valid typed baseline");

        let mut lease_wrong = complete.clone();
        lease_wrong.dispatches[0].lease_id = test_id("task", "wrong-lease");
        lease_wrong.results[0].lease_id = lease_wrong.dispatches[0].lease_id.clone();
        assert!(validate_workgraph_projection(&lease_wrong, &tasks, &artifacts).is_err());

        let mut result_wrong = complete.clone();
        let wrong_result = test_id("task", "wrong-result");
        result_wrong.results[0].result_id = wrong_result.clone();
        result_wrong.evaluations[0].result_id = wrong_result.clone();
        result_wrong.routes[0].result_id = wrong_result;
        assert!(validate_workgraph_projection(&result_wrong, &tasks, &artifacts).is_err());

        let mut evaluation_wrong = complete.clone();
        let wrong_evaluation = test_id("result", "wrong-evaluation");
        evaluation_wrong.evaluations[0].evaluation_id = wrong_evaluation.clone();
        evaluation_wrong.routes[0].evaluation_id = wrong_evaluation;
        assert!(validate_workgraph_projection(&evaluation_wrong, &tasks, &artifacts).is_err());

        let mut route_wrong = complete;
        route_wrong.routes[0].route_id = test_id("evaluation", "wrong-route");
        assert!(validate_workgraph_projection(&route_wrong, &tasks, &artifacts).is_err());
    }

    #[test]
    fn rejected_rework_releases_selected_attempt_and_reuses_assignment_slot() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate first attempt");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("first lease");
        let routed = projection_with_route(
            &first,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            2,
        );
        state
            .reconcile_workgraph(routed.clone(), &documents, 2, now)
            .expect("apply rejected rework route");

        let second = state.workgraph_active.values().next().expect("fresh lease");
        assert_ne!(second.lease_id, first.lease_id);
        assert_eq!(second.attempt, 2);
        assert_eq!(second.assignment_id, first.assignment_id);
        assert_eq!(second.executor_id, first.executor_id);
        assert_eq!(second.slot_id, first.slot_id);
        assert!(!second.has_dispatch);
        assert!(state
            .workgraph_dispatched
            .get("dispatch-1")
            .is_some_and(|lease| !lease.completed && !lease.completion_eligible));

        state
            .reconcile_workgraph(routed, &documents, 3, now)
            .expect("replaying Route is idempotent");
        assert_eq!(state.workgraph_active.len(), 1);
        assert_eq!(
            state
                .workgraph_assignment_attempts
                .get(&test_id("assignment", "assignment"))
                .copied(),
            Some(2)
        );
        state.expire(now + chrono::Duration::minutes(2));
        assert!(state.workgraph_active.is_empty());
        assert!(!state.workgraph_assignments["assignment-comment"].eligible);
        state.validate().expect("rework state");
    }

    #[test]
    fn accepted_route_releases_lease_without_redispatch() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let lease = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("active lease");
        let routed = projection_with_route(
            &lease,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_ACCEPTED,
            "complete",
            3,
        );
        state
            .reconcile_workgraph(routed.clone(), &documents, 2, now)
            .expect("apply accepted route");
        assert!(state.workgraph_active.is_empty());
        assert!(!state.workgraph_assignments["assignment-comment"].eligible);
        assert!(state.workgraph_dispatched["dispatch-1"].route_selected);

        state
            .workgraph_dispatched
            .get_mut("dispatch-1")
            .expect("routed dispatch")
            .route_selected = false;
        let encoded = serde_json::to_vec(&state).expect("checkpoint allocator");
        state = serde_json::from_slice(&encoded).expect("restart allocator");
        state.validate().expect("restored accepted route state");
        let replay = state
            .reconcile_workgraph(routed, &documents, 3, now)
            .expect("accepted Route replay after restart");
        assert!(state.workgraph_dispatched["dispatch-1"].route_selected);
        assert!(replay
            .workgraph_historical
            .iter()
            .any(|lease| lease.route_selected));
        assert!(state.workgraph_active.is_empty());
        assert_eq!(
            state.workgraph_assignment_attempts[&test_id("assignment", "assignment")],
            1
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 4, now)
            .expect("temporary projection gap remains fail closed");
        assert!(state.workgraph_active.is_empty());
        assert!(!state.workgraph_assignments["assignment-comment"].eligible);
        state.validate().expect("accepted route state");
    }

    #[test]
    fn terminal_route_is_removed_when_root_authorization_generation_changes() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let lease = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("lease");
        let routed = projection_with_route(
            &lease,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_ACCEPTED,
            "complete",
            3,
        );
        state
            .reconcile_workgraph(routed, &documents, 2, now)
            .expect("apply terminal Route");
        assert!(state.task_has_terminal_route("issue"));

        let mut roots = BTreeMap::from([("root".to_string(), root_document(false))]);
        let mut revisions = BTreeMap::from([("root".to_string(), 10)]);
        let task_only = projection(Vec::new());
        state.refresh_workgraph_authorizations(
            &task_only,
            &documents,
            &roots,
            &revisions,
            &BTreeMap::new(),
            &[ProjectionInput::RecordIssueRevision {
                source_key: "root".to_string(),
                revision: 10,
                state_fingerprint: "a".repeat(64),
                authorization_transition: true,
            }],
        );
        assert!(state.workgraph_routes.is_empty());
        state
            .reconcile_workgraph_with_roots(task_only.clone(), &documents, &roots, 3, now)
            .expect("exclude task");

        roots.insert("root".to_string(), root_document(true));
        revisions.insert("root".to_string(), 20);
        state.refresh_workgraph_authorizations(
            &task_only,
            &documents,
            &roots,
            &revisions,
            &BTreeMap::new(),
            &[ProjectionInput::RecordIssueRevision {
                source_key: "root".to_string(),
                revision: 20,
                state_fingerprint: "b".repeat(64),
                authorization_transition: true,
            }],
        );
        let mut fresh_assignment = task_only;
        fresh_assignment.assignments[0].source_key = "assignment-comment-new".to_string();
        fresh_assignment.assignments[0].assignment_id = "assignment-new".to_string();
        state
            .reconcile_workgraph_with_roots(fresh_assignment, &documents, &roots, 4, now)
            .expect("reinclude task");
        assert!(
            !state.task_has_terminal_route("issue"),
            "prior generation Route cannot block the new generation"
        );
        assert_eq!(state.workgraph_active.len(), 1);
    }

    #[test]
    fn replayed_rework_restores_retry_after_capacity_returns() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        let configured = AgentFile::from_agents(
            1,
            vec![AgentDefinition {
                agent_id: "executor".to_string(),
                slots: 1,
                lease_duration: "PT1M".to_string(),
                lease_duration_seconds: 60,
            }],
        );
        state.sync_agents(&configured, now);
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("first lease");
        let routed = projection_with_route(
            &first,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            4,
        );
        state
            .reconcile_workgraph(routed.clone(), &documents, 2, now)
            .expect("apply rework");
        state.sync_agents(&AgentFile::from_agents(1, Vec::new()), now);
        state.expire(now + chrono::Duration::minutes(2));
        assert!(state.workgraph_active.is_empty());
        assert!(state.workgraph_assignments["assignment-comment"].eligible);

        state =
            serde_json::from_slice(&serde_json::to_vec(&state).expect("checkpoint capacity gap"))
                .expect("restart during capacity gap");
        state
            .reconcile_workgraph(routed, &documents, 3, now)
            .expect("replay rework without capacity");
        assert!(state.workgraph_assignments["assignment-comment"].eligible);
        assert!(state.workgraph_active.is_empty());

        state.sync_agents(&configured, now);
        let retry = state
            .workgraph_active
            .values()
            .next()
            .expect("retry allocated when capacity returns");
        assert_eq!(retry.assignment_id, first.assignment_id);
        assert!(retry.attempt > first.attempt);
        state.validate().expect("restored rework state");
    }

    #[test]
    fn late_rework_after_expiry_is_persisted_without_releasing_newer_lease() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("first lease");
        state
            .reconcile_workgraph(
                projection(vec![dispatch_binding(&first, "dispatch-1")]),
                &documents,
                2,
                now,
            )
            .expect("accept Dispatch");
        let later = now + chrono::Duration::minutes(2);
        state.expire(later);
        let newer = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("expiry retry");
        assert!(newer.attempt > first.attempt);

        let late = projection_with_route(
            &first,
            "dispatch-1",
            "late-route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            4,
        );
        state
            .reconcile_workgraph(late, &documents, 3, later)
            .expect("retain late valid rework");
        assert_eq!(
            state.workgraph_active.keys().cloned().collect::<Vec<_>>(),
            vec![newer.lease_id]
        );
        let decision = &state.workgraph_routes["late-route-1"];
        assert_eq!(decision.attempt, 1);
        assert_eq!(decision.max_attempts, 4);
        assert_eq!(decision.authorization_generation, 1);
        assert!(!state.task_has_terminal_route("issue"));
        state.validate().expect("late rework state");
    }

    #[test]
    fn replayed_rework_cannot_override_later_terminal_route() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("first lease");
        let rework = projection_with_route(
            &first,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            4,
        );
        state
            .reconcile_workgraph(rework.clone(), &documents, 2, now)
            .expect("apply rework");
        let retry = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("retry lease");
        let terminal = projection_with_route(
            &retry,
            "dispatch-2",
            "route-2",
            WORKGRAPH_EVALUATION_ACCEPTED,
            "complete",
            4,
        );
        state
            .reconcile_workgraph(terminal, &documents, 3, now)
            .expect("apply terminal Route");
        assert!(state.workgraph_active.is_empty());
        assert!(state.task_has_terminal_route("issue"));

        state
            .reconcile_workgraph(rework, &documents, 4, now)
            .expect("replay old rework");
        assert!(state.workgraph_active.is_empty());
        assert!(!state.workgraph_assignments["assignment-comment"].eligible);
        state.validate().expect("terminal supersedes rework");
    }

    #[test]
    fn replayed_rework_cannot_reopen_closed_task() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("first lease");
        let rework = projection_with_route(
            &first,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            4,
        );
        state
            .reconcile_workgraph(rework.clone(), &documents, 2, now)
            .expect("apply rework");
        documents.get_mut("issue").expect("task").is_open = false;
        state
            .reconcile_workgraph(rework, &documents, 3, now)
            .expect("close while replaying rework");
        assert!(state.workgraph_active.is_empty());
        assert!(!state.workgraph_assignments["assignment-comment"].eligible);
        state.validate().expect("closed task remains closed");
    }

    #[test]
    fn dispatch_and_terminal_route_coalesce_to_inactive_historical_delta() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let expected = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("active lease");
        let routed = projection_with_route(
            &expected,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_ACCEPTED,
            "complete",
            3,
        );
        let delta = state
            .reconcile_workgraph(routed, &documents, 2, now)
            .expect("dispatch and Route in one transition");
        assert!(state.workgraph_active.is_empty());
        assert!(delta.workgraph_started.is_empty());
        assert_eq!(delta.workgraph_released.len(), 1);
        assert!(delta.workgraph_ended.is_empty());
        assert_eq!(delta.workgraph_historical.len(), 1);
        assert!(!delta.workgraph_historical[0].completed);
        assert!(delta.workgraph_historical[0].completion_eligible);
        let changes = allocation_changes("source", 2, &delta, &state.agent_runtime());
        let lease_states = changes
            .iter()
            .filter_map(|change| match change {
                SourceChange::Update {
                    element: drasi_core::models::Element::Node { properties, .. },
                } => properties.get("active"),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(lease_states
            .iter()
            .any(|value| **value == drasi_core::models::ElementValue::Bool(false)));
        assert!(!lease_states
            .iter()
            .any(|value| **value == drasi_core::models::ElementValue::Bool(true)));
        let selected_states = changes
            .iter()
            .filter_map(|change| match change {
                SourceChange::Update {
                    element: drasi_core::models::Element::Node { properties, .. },
                    ..
                } => properties.get("selected"),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(selected_states
            .iter()
            .any(|value| **value == drasi_core::models::ElementValue::Bool(true)));
    }

    #[test]
    fn stale_prior_route_cannot_release_rework_attempt() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let documents = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let mut state = AllocationState::default();
        authorize_task(&mut state, 1);
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("allocate");
        let first = state
            .workgraph_active
            .values()
            .next()
            .cloned()
            .expect("first lease");
        let route = projection_with_route(
            &first,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            3,
        );
        state
            .reconcile_workgraph(route, &documents, 2, now)
            .expect("rework");
        let second_id = state
            .workgraph_active
            .values()
            .next()
            .expect("second lease")
            .lease_id
            .clone();
        let stale = projection_with_route(
            &first,
            "dispatch-1",
            "late-route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            3,
        );
        state
            .reconcile_workgraph(stale, &documents, 3, now)
            .expect("stale Route is retained but inert");
        assert_eq!(
            state.workgraph_active.keys().cloned().collect::<Vec<_>>(),
            vec![second_id]
        );
        assert_eq!(
            state
                .workgraph_routes
                .get("late-route-1")
                .expect("late rework decision")
                .attempt,
            1
        );
    }

    #[test]
    fn route_validation_rejects_wrong_chain_and_rework_at_attempt_limit() {
        let lease = WorkGraphActiveLease {
            dispatch_id: String::new(),
            actor_kind: ActorKind::Agent,
            actor_github: None,
            lease_id: make_lease_id(TEST_TASK_ID, &test_id("assignment", "assignment"), 1),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            task_element_id: "task-element".to_string(),
            assignment_source_key: "assignment-comment".to_string(),
            assignment_id: test_id("assignment", "assignment"),
            executor_id: "executor".to_string(),
            slot_id: "executor:1".to_string(),
            slot_number: 1,
            attempt: 1,
            acquired_at: "2026-01-01T00:00:00.000Z".to_string(),
            expires_at: "2026-01-01T00:01:00.000Z".to_string(),
            has_dispatch: true,
            completed: false,
            completion_eligible: true,
            route_selected: false,
        };
        let mut desired = projection_with_route(
            &lease,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            2,
        );
        let tasks = BTreeMap::from([("issue".to_string(), task_document(true))]);
        let artifacts = lifecycle_artifacts_for_projection(&desired);
        validate_workgraph_projection(&desired, &tasks, &artifacts).expect("valid chain");

        desired.routes[0].result_id = "other-result".to_string();
        assert!(validate_workgraph_projection(&desired, &tasks, &artifacts).is_err());
        desired.routes[0].result_id = test_id("result", "result-1");
        desired.routes[0].max_attempts = 1;
        assert!(validate_workgraph_projection(&desired, &tasks, &artifacts).is_err());
        desired.routes[0].action = "error".to_string();
        validate_workgraph_projection(&desired, &tasks, &artifacts)
            .expect("bounded workflow may signal its error Route at the limit");
        desired.routes[0].action = WORKGRAPH_ROUTE_REWORK.to_string();
        desired.routes[0].max_attempts = 2;
        desired.evaluations[0].verdict = WORKGRAPH_EVALUATION_ACCEPTED.to_string();
        assert!(validate_workgraph_projection(&desired, &tasks, &artifacts).is_err());
    }

    #[test]
    fn route_document_arriving_first_survives_restart_and_later_converges() {
        let lease = WorkGraphActiveLease {
            dispatch_id: String::new(),
            actor_kind: ActorKind::Agent,
            actor_github: None,
            lease_id: make_lease_id(TEST_TASK_ID, &test_id("assignment", "assignment"), 1),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            task_element_id: "task-element".to_string(),
            assignment_source_key: "assignment-comment".to_string(),
            assignment_id: test_id("assignment", "assignment"),
            executor_id: "executor".to_string(),
            slot_id: "executor:1".to_string(),
            slot_number: 1,
            attempt: 1,
            acquired_at: "2026-01-01T00:00:00.000Z".to_string(),
            expires_at: "2026-01-01T00:01:00.000Z".to_string(),
            has_dispatch: true,
            completed: false,
            completion_eligible: true,
            route_selected: false,
        };
        let desired = projection_with_route(
            &lease,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_ACCEPTED,
            "complete",
            3,
        );
        let complete_artifacts = lifecycle_artifacts_for_projection(&desired);
        let route = complete_artifacts["route-1"].clone();
        let (_, _, _, _, _, artifacts, artifact_revisions) = stage_workgraph_documents(
            &AllocationState::default(),
            &[ProjectionInput::UpsertLifecycleArtifact(route)],
        );
        assert_eq!(artifacts.len(), 1);

        let mut restarted = AllocationState::default();
        restarted.workgraph_artifacts = artifacts;
        restarted.workgraph_artifact_revisions = artifact_revisions;
        restarted = serde_json::from_slice(
            &serde_json::to_vec(&restarted).expect("checkpoint out-of-order Route"),
        )
        .expect("restart before dependencies");
        let mut tasks = BTreeMap::new();
        let mut artifacts = restarted.workgraph_artifacts.clone();
        let mut artifact_revisions = restarted.workgraph_artifact_revisions.clone();
        apply_workgraph_documents(
            &complete_artifacts
                .values()
                .filter(|artifact| artifact.source_key != "route-1")
                .cloned()
                .map(ProjectionInput::UpsertLifecycleArtifact)
                .chain(std::iter::once(ProjectionInput::UpsertTask(task_document(
                    true,
                ))))
                .collect::<Vec<_>>(),
            &mut BTreeMap::new(),
            &mut BTreeMap::new(),
            &mut BTreeMap::new(),
            &mut BTreeMap::new(),
            &mut tasks,
            &mut artifacts,
            &mut artifact_revisions,
        );
        validate_workgraph_projection(&desired, &tasks, &artifacts)
            .expect("full authoritative chain converges");
    }

    #[test]
    fn stale_generation_fences_result_evaluation_and_route_chain() {
        let lease = WorkGraphActiveLease {
            dispatch_id: String::new(),
            actor_kind: ActorKind::Agent,
            actor_github: None,
            lease_id: make_lease_id(TEST_TASK_ID, &test_id("assignment", "assignment"), 1),
            task_source_key: "issue".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            task_id: TEST_TASK_ID.to_string(),
            task_element_id: "task-element".to_string(),
            assignment_source_key: "assignment-comment".to_string(),
            assignment_id: test_id("assignment", "assignment"),
            executor_id: "executor".to_string(),
            slot_id: "executor:1".to_string(),
            slot_number: 1,
            attempt: 1,
            acquired_at: "2026-01-01T00:00:00.000Z".to_string(),
            expires_at: "2026-01-01T00:01:00.000Z".to_string(),
            has_dispatch: true,
            completed: false,
            completion_eligible: true,
            route_selected: false,
        };
        let mut desired = projection_with_route(
            &lease,
            "dispatch-1",
            "route-1",
            WORKGRAPH_EVALUATION_REJECTED,
            WORKGRAPH_ROUTE_REWORK,
            3,
        );
        let mut state = AllocationState::default();
        state.workgraph_authorizations.insert(
            "issue".to_string(),
            WorkGraphAuthorizationState {
                root_issue_id: "root".to_string(),
                generation: 2,
                cutoff_revision: 20,
                transition_revision: 20,
                included: true,
            },
        );
        state.workgraph_artifacts = lifecycle_artifacts_for_projection(&desired);
        refresh_artifact_revisions(&mut state);
        state.workgraph_artifact_generations.extend(
            state
                .workgraph_artifacts
                .keys()
                .map(|source_key| (source_key.clone(), 1)),
        );

        state.fence_stale_workgraph_authorizations(&mut desired);
        assert!(desired.assignments.is_empty());
        assert!(desired.dispatches.is_empty());
        assert!(desired.results.is_empty());
        assert!(desired.evaluations.is_empty());
        assert!(desired.routes.is_empty());
    }

    #[test]
    fn queued_assignment_marks_affected_executor_without_a_free_slot() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp");
        let mut state = AllocationState::default();
        state.sync_agents(
            &AgentFile::from_agents(
                1,
                vec![AgentDefinition {
                    agent_id: "executor".to_string(),
                    slots: 1,
                    lease_duration: "PT1M".to_string(),
                    lease_duration_seconds: 60,
                }],
            ),
            now,
        );
        let documents = BTreeMap::from([
            (
                "issue".to_string(),
                TaskDocument {
                    assignees: Vec::new(),
                    source_key: "issue".to_string(),
                    body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: None,
                    workgraph_labels: Vec::new(),
                    workgraph_include: true,
                },
            ),
            (
                "issue-2".to_string(),
                TaskDocument {
                    assignees: Vec::new(),
                    source_key: "issue-2".to_string(),
                    body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: None,
                    workgraph_labels: Vec::new(),
                    workgraph_include: true,
                },
            ),
        ]);
        state
            .reconcile_workgraph(projection(Vec::new()), &documents, 1, now)
            .expect("occupy only slot");

        let mut queued = projection(Vec::new());
        queued.tasks.push(WorkGraphTaskBinding {
            source_key: "issue-2".to_string(),
            task_id: TEST_TASK_2_ID.to_string(),
            task_element_id: "task-element-2".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
        });
        queued.assignments.push(WorkGraphAssignmentBinding {
            source_key: "assignment-comment-2".to_string(),
            task_source_key: "issue-2".to_string(),
            task_id: TEST_TASK_2_ID.to_string(),
            assignment_id: test_id("assignment", "assignment-2"),
            permitted_executors: vec!["executor".to_string()],
            root_issue_id: "root".to_string(),
            workflow_run_id: test_id("workflow-run", "run"),
            request_id: None,
            response_id: None,
            assigner_id: None,
        });
        let delta = state
            .reconcile_workgraph(queued, &documents, 2, now + chrono::Duration::seconds(1))
            .expect("queue second assignment");
        assert!(delta.affected_agents.contains("executor"));
        assert_eq!(state.agent_runtime()["executor"].queue_depth, 1);
    }
}
