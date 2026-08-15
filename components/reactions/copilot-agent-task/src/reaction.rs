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

//! The Copilot Agent Task reaction.
//!
//! Each added launch-query row **is** one authoritative `ResponsibilityAssigned`
//! comment as the GitHub Source projected it (see [`crate::row`]). For each such
//! row the reaction:
//!
//! 1. validates the row against the configured allowlists and accepts its
//!    assignment event — unedited (`isEdited == false`), authored by
//!    `trustedAssignmentAuthorDatabaseId` + `trustedAssignmentAuthorType`,
//!    strictly parsed, and bound to the row's item, subject, and `bodyDigest`;
//! 2. re-reads the authoritative issue and requires the **current** body digest
//!    to still equal the row's `bodyDigest` (a body edited since the assignment
//!    aborts the launch with no effect);
//! 3. verifies the Project item binding and that its status is
//!    `AwaitingValidation`;
//! 4. confirms the named assignment comment is still present, trusted, unedited,
//!    and carries exactly the event the row delivered;
//! 5. pins the agent profile to the exact blob the assignment named;
//! 6. durably reserves the run **before** any external write;
//! 7. reconciles-or-creates exactly one agent task (with one fallback-model
//!    retry on a clearly-unsupported-model 422); and
//! 8. posts exactly one `ExecutionStarted` WorkGraphEvent/v1 comment, adopting
//!    one a previous attempt may already have written only when its canonical
//!    event JSON is byte-identical to the event this reaction intends to post.
//!
//! Each side effect is reconciled and persisted with an exact-bytes
//! compare-and-swap before the next, so a crash at any point resumes without
//! duplicating an external effect. `Update`/`Delete`/`Aggregation`/`Noop`
//! diffs are never acted on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{error, info, warn};

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::CheckpointState;
use drasi_lib::reactions::ManagerCheckpointOwnership;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Reaction;
use drasi_workgraph_common::{
    comment::{parse_comment, render_comment},
    dedup::{adopt_published_event, ObservedComment},
    event::{
        ExecutionId, ExecutionStartedPayload, ProfileRef, ResponsibilityAssignedPayload, RunId,
        Sha256Digest, WorkGraphEvent, WorkGraphEventPayload, WorkGraphEventType,
    },
    ids::{body_digest, event_id},
    status::AWAITING_VALIDATION,
    summary::summary_for,
};

use crate::config::CopilotAgentTaskReactionConfig;
use crate::github::{
    CreateTaskOutcome, CreateTaskRequest, GitHubClient, GitHubConfig, IssueComment,
    ReconciliationOutcome,
};
use crate::ids::execution_id;
use crate::prompt::build_prompt;
use crate::row::LaunchRow;
use crate::state::{
    compare_and_swap_record, create_record_if_absent, load_record, ExecutionRecord,
    PersistedExecutionRecord, WorkGraphExecutionStateV1,
};
use crate::CopilotAgentTaskReactionBuilder;

/// A row that can never succeed, no matter how often it is retried.
///
/// Permanent rejections are logged and skipped; they have no external effect,
/// so unlike transient failures they do not need a durable tombstone to stay
/// consistent across replays.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct PermanentCandidateError {
    message: String,
}

impl PermanentCandidateError {
    #[allow(clippy::new_ret_no_self)]
    fn new(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            message: message.into(),
        })
    }
}

pub struct CopilotAgentTaskReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: CopilotAgentTaskReactionConfig,
}

impl CopilotAgentTaskReaction {
    pub fn builder(id: impl Into<String>) -> CopilotAgentTaskReactionBuilder {
        CopilotAgentTaskReactionBuilder::new(id)
    }

    /// Construct directly from a config, validating it against `queries`.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: CopilotAgentTaskReactionConfig,
    ) -> Result<Self> {
        config.validate(&queries)?;
        let params = ReactionBaseParams::new(id.into(), queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: CopilotAgentTaskReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    fn github_client(&self) -> Result<GitHubClient> {
        GitHubClient::new(GitHubConfig {
            api_base_url: self.config.github_api_base_url.clone(),
            graphql_url: self.config.github_graphql_url.clone(),
            agent_tasks_api_version: self.config.agent_tasks_api_version.clone(),
            token: self.config.token.clone(),
            request_timeout_ms: self.config.request_timeout_ms,
        })
    }
}

#[async_trait]
impl Reaction for CopilotAgentTaskReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "copilot-agent-task"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let dto = crate::descriptor::CopilotAgentTaskReactionConfigDto::from(&self.config);
        self.base.properties_or_serialize(&dto)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("Copilot Agent Task Reaction", &self.base.id);
        info!(
            "[{}] Copilot Agent Task reaction starting - API base: {}",
            self.base.id, self.config.github_api_base_url
        );

        if let Err(e) = self.config.validate(&self.base.queries) {
            error!(
                "[{}] Invalid Copilot Agent Task reaction config: {e:#}",
                self.base.id
            );
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Invalid configuration: {e:#}")),
                )
                .await;
            return Err(e);
        }

        // Durable state store is required: reservation records must survive
        // restarts for idempotency to hold.
        let state_store = self.base.state_store().await;
        if state_store
            .as_deref()
            .is_none_or(|store| !store.is_durable())
        {
            let msg = "Copilot Agent Task reaction requires a durable state store";
            error!("[{}] {msg}", self.base.id);
            self.base
                .set_status(ComponentStatus::Error, Some(msg.to_string()))
                .await;
            anyhow::bail!(msg);
        }

        let client = match self.github_client() {
            Ok(c) => c,
            Err(e) => {
                error!("[{}] Failed to create GitHub client: {e}", self.base.id);
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Failed to create GitHub client: {e}")),
                    )
                    .await;
                return Err(e);
            }
        };
        if let Some(expected_user_id) = &self.config.expected_github_user_id {
            let actual_user_id = match client.authenticated_user_id().await {
                Ok(user_id) => user_id,
                Err(error) => {
                    let message = format!("failed to verify GitHub token identity: {error}");
                    self.base
                        .set_status(ComponentStatus::Error, Some(message.clone()))
                        .await;
                    anyhow::bail!(message);
                }
            };
            if actual_user_id != *expected_user_id {
                let message = format!(
                    "GitHub token user ID mismatch: expected {expected_user_id}, authenticated as {actual_user_id}"
                );
                self.base
                    .set_status(ComponentStatus::Error, Some(message.clone()))
                    .await;
                anyhow::bail!(message);
            }
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting Copilot Agent Task reaction".to_string()),
            )
            .await;

        let shutdown_rx = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let config = self.config.clone();
        let reaction_name = self.base.id.clone();

        let handle = tokio::spawn(async move {
            let mut checkpoint_state = CheckpointState::load(&base).await;
            run_processing_loop(
                &reaction_name,
                base,
                config,
                Arc::new(client),
                &mut checkpoint_state,
                shutdown_rx,
            )
            .await;
        });

        self.base.set_processing_task(handle).await;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Copilot Agent Task reaction running".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }

    /// Reservation/execution state must survive restarts for idempotency.
    fn is_durable(&self) -> bool {
        true
    }

    /// A trigger reaction: launching on the entire historical result set on a
    /// fresh start would fire duplicate task creation for already-handled rows.
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    /// Always Strict: an ambiguous or failed launch must stop the pipeline for
    /// reconciliation rather than silently skip or reset.
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }

    fn checkpoint_ownership(&self) -> ManagerCheckpointOwnership {
        ManagerCheckpointOwnership::Reaction
    }
}

async fn run_processing_loop(
    reaction_name: &str,
    base: ReactionBase,
    config: CopilotAgentTaskReactionConfig,
    github: Arc<GitHubClient>,
    checkpoint_state: &mut CheckpointState,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        let event = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                info!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                break;
            }
            event = base.priority_queue.dequeue() => event,
        };

        if let Err(error) =
            process_query_result(reaction_name, &base, &config, &github, &event).await
        {
            error!(
                "[{reaction_name}] launch failed for query '{}' sequence {}: {error:#}",
                event.query_id, event.sequence
            );
            base.set_status(
                ComponentStatus::Error,
                Some(format!("Copilot Agent Task failed: {error:#}")),
            )
            .await;
            // Strict recovery: stop without advancing the checkpoint so the
            // batch replays from the outbox after operator intervention.
            return;
        }

        if let Err(error) = checkpoint_state
            .advance(&base, &event.query_id, event.sequence)
            .await
        {
            error!(
                "[{reaction_name}] checkpoint update failed at sequence {}: {error:#}",
                event.sequence
            );
            base.set_status(
                ComponentStatus::Error,
                Some(format!("Copilot Agent Task checkpoint failure: {error:#}")),
            )
            .await;
            return;
        }
    }

    info!("[{reaction_name}] Copilot Agent Task processing loop stopped");
}

async fn process_query_result(
    reaction_name: &str,
    base: &ReactionBase,
    config: &CopilotAgentTaskReactionConfig,
    github: &GitHubClient,
    result: &QueryResult,
) -> Result<()> {
    let store = base.state_store().await.ok_or_else(|| {
        anyhow::anyhow!("a durable state store is required for the Copilot Agent Task reaction")
    })?;
    let ctx = LaunchCtx {
        reaction_name,
        base,
        config,
        github,
        store,
    };

    for diff in &result.results {
        match diff {
            ResultDiff::Add { data, .. } => {
                let row: LaunchRow = match LaunchRow::from_json(data) {
                    Ok(row) => row,
                    Err(error) => {
                        warn!(
                            "[{reaction_name}] skipping malformed launch row on query '{}': {error:#}",
                            result.query_id
                        );
                        continue;
                    }
                };
                match ctx.launch(&row).await {
                    Ok(()) => {}
                    Err(error) if error.downcast_ref::<PermanentCandidateError>().is_some() => {
                        warn!(
                            "[{reaction_name}] rejecting launch row for {}#{}: {error}",
                            row.repository, row.subject_number
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            ResultDiff::Update { .. } | ResultDiff::Delete { .. } => {
                info!(
                    "[{reaction_name}] ignoring non-added diff for query '{}'",
                    result.query_id
                );
            }
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
        }
    }
    Ok(())
}

/// The invariant context shared by every step of one launch batch.
struct LaunchCtx<'a> {
    reaction_name: &'a str,
    base: &'a ReactionBase,
    config: &'a CopilotAgentTaskReactionConfig,
    github: &'a GitHubClient,
    store: Arc<dyn StateStoreProvider>,
}

impl LaunchCtx<'_> {
    fn store_id(&self) -> &str {
        &self.base.id
    }

    /// Launch one assigned run, resuming any partially completed prior attempt.
    async fn launch(&self, row: &LaunchRow) -> Result<()> {
        // 0. Derive the durable identity without consulting mutable issue,
        //    Project, profile, or comment state. A pre-existing nonterminal
        //    record owns recovery and must never be abandoned because those live
        //    preconditions changed after an external write.
        let digest = Sha256Digest::try_from(row.body_digest.clone())
            .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
        let run = RunId::new(&row.project_item_node_id, &digest)
            .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
        let execution = execution_id(&run);
        if let Some(mut persisted) =
            load_record(self.store.clone(), self.store_id(), run.as_str()).await?
        {
            persisted.record.ensure_matches(run.as_str(), row)?;
            if persisted.record.is_terminal() {
                info!(
                    "[{}] run '{run}' for {}#{} is already {:?}; nothing to do",
                    self.reaction_name,
                    persisted.record.repository,
                    persisted.record.subject_number,
                    persisted.record.status
                );
                return Ok(());
            }
            return self.resume_persisted(&mut persisted).await;
        }

        // 1. Validate the row against the configured allowlists, then accept the
        //    assignment event it carries: unedited, authored by the trusted
        //    assignment identity, strictly parsed, and bound to this row's item,
        //    subject, and issue-body digest.
        row.validate(self.config)
            .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
        let (owner, repo) = row
            .owner_and_repo()
            .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
        let assignment = row
            .accept_assignment(self.config)
            .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
        if assignment.run_id != run || assignment.body_digest != digest {
            return Err(PermanentCandidateError::new(
                "assignment identity does not match the row-derived durable run",
            ));
        }
        let payload = match &assignment.event.payload {
            WorkGraphEventPayload::ResponsibilityAssigned(payload) => payload.clone(),
            other => {
                // Unreachable: acceptance already pinned the event type.
                return Err(PermanentCandidateError::new(format!(
                    "assignment row carries a {} payload, not ResponsibilityAssigned",
                    other.event_type()
                )));
            }
        };

        // 2. Authoritative issue read: the row's `bodyDigest` must still be the
        //    issue's current digest, or the run this row names no longer exists.
        let issue = self
            .github
            .issue_snapshot(&row.repository, row.subject_number)
            .await
            .context("failed to read the authoritative issue")?;
        if !issue.state.eq_ignore_ascii_case("open") {
            return Err(PermanentCandidateError::new(format!(
                "{}#{} is '{}', not open",
                row.repository, row.subject_number, issue.state
            )));
        }
        if issue.node_id != row.subject_node_id {
            return Err(PermanentCandidateError::new(format!(
                "{}#{} resolves to node '{}', not the row's '{}'",
                row.repository, row.subject_number, issue.node_id, row.subject_node_id
            )));
        }
        let current_digest = body_digest(issue.body.as_deref());
        if current_digest != digest {
            return Err(PermanentCandidateError::new(format!(
                "issue body changed since the assignment: row bodyDigest '{}' but the current body is '{}'",
                digest.as_str(),
                current_digest.as_str()
            )));
        }
        if payload.content_digest != digest {
            return Err(PermanentCandidateError::new(
                "assignment content digest does not match the current issue body",
            ));
        }

        // 3. Verify the Project item binding and status.
        self.verify_project(row).await?;

        // 4. Confirm the named assignment comment is still exactly what the row
        //    delivered. The row is authoritative about *which* event to act on;
        //    this proves that comment has not since been edited, deleted, or
        //    replaced by different content.
        let comments = self
            .github
            .list_issue_comments(&row.repository, row.subject_number)
            .await
            .context("failed to list issue comments")?;
        self.verify_assignment_comment(row, &assignment.event, &comments)?;

        // 5. Pin the profile to the exact blob the assignment named.
        let profile = payload.profile_ref.profile().to_string();
        if !self.config.allowed_profiles.iter().any(|p| p == &profile) {
            return Err(PermanentCandidateError::new(format!(
                "assignment profile '{profile}' is not in allowedProfiles"
            )));
        }
        let path = format!(".github/agents/{profile}.agent.md");
        let live_sha = self
            .github
            .blob_sha_at_path(owner, repo, &path, &row.base_ref)
            .await
            .context("failed to pin the agent profile blob")?;
        match live_sha.as_deref() {
            Some(sha) if sha == payload.profile_ref.blob_sha() => {}
            other => {
                return Err(PermanentCandidateError::new(format!(
                    "profile '{profile}' blob SHA {other:?} does not match the assignment's pin '{}'",
                    payload.profile_ref.blob_sha()
                )));
            }
        }

        // 6. Durable reservation before any external write.
        let started_event_id = event_id(&run, WorkGraphEventType::ExecutionStarted);
        let intent = ExecutionRecord::new(
            run.as_str(),
            started_event_id.as_str(),
            execution.as_str(),
            row,
            digest.as_str(),
            payload.profile_ref.as_str(),
        );
        let mut persisted =
            match create_record_if_absent(self.store.clone(), self.store_id(), &intent).await? {
                Some(existing) => {
                    existing.record.ensure_matches(run.as_str(), row)?;
                    existing
                }
                None => load_record(self.store.clone(), self.store_id(), run.as_str())
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("execution record vanished immediately after create")
                    })?,
            };

        self.resume_persisted(&mut persisted).await
    }

    /// Resume exclusively from pinned durable intent.
    async fn resume_persisted(&self, persisted: &mut PersistedExecutionRecord) -> Result<()> {
        let run = RunId::try_from(persisted.record.run_id.clone())?;
        let execution = ExecutionId::try_from(persisted.record.execution_id.clone())?;
        let expected_execution = execution_id(&run);
        if execution != expected_execution {
            anyhow::bail!("persisted execution ID '{execution}' does not match run '{run}'");
        }
        let expected_event_id = event_id(&run, WorkGraphEventType::ExecutionStarted);
        let persisted_event_id = &persisted.record.event_id;
        if persisted_event_id != expected_event_id.as_str() {
            anyhow::bail!(
                "persisted ExecutionStarted event ID '{persisted_event_id}' does not match run '{run}'"
            );
        }
        let profile_ref = ProfileRef::try_from(persisted.record.profile_ref.clone())?;
        let profile = profile_ref.profile().to_string();
        let (owner, repo) = persisted
            .record
            .repository
            .split_once('/')
            .filter(|(owner, repo)| !owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "persisted repository '{}' is not 'owner/repo'",
                    persisted.record.repository
                )
            })?;
        let owner = owner.to_string();
        let repo = repo.to_string();
        let prompt = build_prompt(persisted.record.subject_number, execution.as_str());

        if !self
            .provision_task(&owner, &repo, &execution, &profile, &prompt, persisted)
            .await?
        {
            return Ok(());
        }
        self.post_execution_started(&run, &execution, persisted)
            .await?;

        info!(
            "[{}] launched {}#{} as run '{run}' (execution {})",
            self.reaction_name,
            persisted.record.repository,
            persisted.record.subject_number,
            execution.as_str()
        );
        Ok(())
    }

    /// Require the named assignment and every trusted duplicate of its
    /// deterministic event ID to agree before any external effect.
    fn verify_assignment_comment(
        &self,
        row: &LaunchRow,
        accepted: &WorkGraphEvent,
        comments: &[IssueComment],
    ) -> Result<()> {
        let observed = self.observed_comments(comments, &self.config.trusted_assignment_author());
        let named = observed
            .iter()
            .find(|comment| comment.comment_node_id == row.event_comment_node_id)
            .ok_or_else(|| {
                PermanentCandidateError::new(format!(
                    "assignment comment '{}' is missing, edited, untrusted, or noncanonical on {}#{}",
                    row.event_comment_node_id, row.repository, row.subject_number
                ))
            })?;
        if named.comment.event != *accepted {
            anyhow::bail!(
                "assignment comment '{}' no longer carries the event the row delivered",
                row.event_comment_node_id
            );
        }

        adopt_published_event(&observed, accepted)
            .map_err(|error| anyhow::anyhow!("assignment reconciliation failed: {error}"))?
            .ok_or_else(|| anyhow::anyhow!("trusted assignment event disappeared"))?;
        Ok(())
    }

    /// Verify the Project item binding and that its status is
    /// `AwaitingValidation`. Every mismatch is a permanent semantic rejection.
    async fn verify_project(&self, row: &LaunchRow) -> Result<()> {
        let snapshot = self
            .github
            .project_snapshot(&row.project_node_id, &row.project_item_node_id)
            .await
            .context("failed to read the project snapshot")?;
        if snapshot.item_project_node_id != row.project_node_id {
            return Err(PermanentCandidateError::new(format!(
                "project item '{}' belongs to project '{}', not the row's '{}'",
                row.project_item_node_id, snapshot.item_project_node_id, row.project_node_id
            )));
        }
        if snapshot.content_type.as_deref() != Some("Issue") {
            return Err(PermanentCandidateError::new(format!(
                "project item content type {:?} is not Issue",
                snapshot.content_type
            )));
        }
        if snapshot.content_issue_node_id.as_deref() != Some(row.subject_node_id.as_str()) {
            return Err(PermanentCandidateError::new(format!(
                "project item is linked to issue {:?}, not the row's '{}'",
                snapshot.content_issue_node_id, row.subject_node_id
            )));
        }
        if snapshot.content_number != Some(row.subject_number) {
            return Err(PermanentCandidateError::new(format!(
                "project item issue number {:?} does not match the row's {}",
                snapshot.content_number, row.subject_number
            )));
        }
        if snapshot.content_repository.as_deref() != Some(row.repository.as_str()) {
            return Err(PermanentCandidateError::new(format!(
                "project item repository {:?} does not match the row's '{}'",
                snapshot.content_repository, row.repository
            )));
        }
        if snapshot.status_field_node_id.as_deref()
            != Some(self.config.expected_project_status_field_node_id.as_str())
        {
            return Err(PermanentCandidateError::new(format!(
                "project status field {:?} does not match expected '{}'",
                snapshot.status_field_node_id, self.config.expected_project_status_field_node_id
            )));
        }
        if snapshot.current_status.as_deref() != Some(AWAITING_VALIDATION) {
            return Err(PermanentCandidateError::new(format!(
                "project item status {:?} is not '{AWAITING_VALIDATION}'",
                snapshot.current_status
            )));
        }
        Ok(())
    }

    /// Reconcile-or-create exactly one agent task.
    ///
    /// Returns `Ok(true)` when a task is confirmed (created or adopted) and
    /// `Ok(false)` when a terminal create-task rejection was durably recorded
    /// (the run is permanently skipped). Transient/ambiguous outcomes return
    /// `Err` to halt for reconciliation on restart.
    async fn provision_task(
        &self,
        owner: &str,
        repo: &str,
        execution: &ExecutionId,
        profile: &str,
        prompt: &str,
        persisted: &mut PersistedExecutionRecord,
    ) -> Result<bool> {
        if persisted.record.task_confirmed() {
            return Ok(true);
        }
        let execution = execution.as_str();

        let attempts = if persisted.record.ambiguous {
            self.config.comment_api.max_attempts.max(2)
        } else {
            1
        };
        let backoff = Duration::from_millis(self.config.comment_api.retry_backoff_ms);
        for attempt in 1..=attempts {
            match self
                .github
                .reconcile(owner, repo, execution)
                .await
                .map_err(|error| anyhow::anyhow!("task reconciliation failed: {error}"))?
            {
                ReconciliationOutcome::ExactMatch(task) => {
                    let model = persisted
                        .record
                        .model_used
                        .clone()
                        .unwrap_or_else(|| persisted.record.requested_model.clone());
                    let used_fallback = persisted.record.used_fallback;
                    let mut updated = persisted.record.clone();
                    let url = (!task.url.is_empty()).then_some(task.url);
                    updated.set_task(model, used_fallback, task.id, url);
                    self.persist(persisted, updated).await?;
                    info!(
                        "[{}] adopted existing task for execution {execution}",
                        self.reaction_name
                    );
                    return Ok(true);
                }
                ReconciliationOutcome::Ambiguous(matches) if matches.len() >= 2 => {
                    anyhow::bail!(
                        "reconciliation found {} tasks correlated to execution {execution}; failing closed",
                        matches.len()
                    );
                }
                ReconciliationOutcome::Ambiguous(_) if persisted.record.ambiguous => {
                    if attempt < attempts {
                        tokio::time::sleep(backoff).await;
                    }
                }
                ReconciliationOutcome::Ambiguous(_) => break,
            }
        }
        if persisted.record.ambiguous {
            anyhow::bail!(
                "ambiguous task creation for execution {execution} remains unresolved after {attempts} authoritative reads; refusing to recreate"
            );
        }

        self.create_task_with_fallback(owner, repo, profile, prompt, persisted)
            .await
    }

    /// Create the task, retrying exactly once with the fallback model on a
    /// clearly-unsupported-model 422.
    async fn create_task_with_fallback(
        &self,
        owner: &str,
        repo: &str,
        profile: &str,
        prompt: &str,
        persisted: &mut PersistedExecutionRecord,
    ) -> Result<bool> {
        let requested_model = persisted.record.requested_model.clone();
        let fallback_model = persisted.record.fallback_model.clone();
        let base_ref = persisted.record.base_ref.clone();
        // Persist the chosen model BEFORE the attempt so a crash mid-flight
        // leaves the in-flight model on record for reconciliation.
        self.record_attempt_model(persisted, &requested_model, false)
            .await?;
        self.mark_ambiguous(
            persisted,
            format!("task creation outcome pending for model '{requested_model}'"),
        )
        .await?;
        match self
            .create_task(owner, repo, profile, &requested_model, &base_ref, prompt)
            .await
        {
            CreateTaskOutcome::Created { id, url } => {
                self.confirm_task(persisted, &requested_model, false, id, url)
                    .await?;
                Ok(true)
            }
            CreateTaskOutcome::Permanent(message) => {
                self.fail(persisted, format!("create task rejected: {message}"))
                    .await?;
                Ok(false)
            }
            CreateTaskOutcome::Transient(message) => {
                self.record_attempt_model(persisted, &requested_model, false)
                    .await?;
                anyhow::bail!("create task failed transiently: {message}")
            }
            CreateTaskOutcome::Ambiguous => {
                self.mark_ambiguous(persisted, "create task outcome ambiguous (transport error)")
                    .await?;
                anyhow::bail!("create task outcome ambiguous; awaiting reconciliation on restart")
            }
            CreateTaskOutcome::UnsupportedModel(message) => {
                let Some(fallback) = fallback_model else {
                    self.fail(
                        persisted,
                        format!(
                            "requested model unsupported and no fallback configured: {message}"
                        ),
                    )
                    .await?;
                    return Ok(false);
                };
                warn!(
                    "[{}] requested model unsupported; retrying once with the fallback model",
                    self.reaction_name
                );
                self.record_attempt_model(persisted, &fallback, true)
                    .await?;
                self.mark_ambiguous(
                    persisted,
                    format!("task creation outcome pending for fallback model '{fallback}'"),
                )
                .await?;
                match self
                    .create_task(owner, repo, profile, &fallback, &base_ref, prompt)
                    .await
                {
                    CreateTaskOutcome::Created { id, url } => {
                        self.confirm_task(persisted, &fallback, true, id, url)
                            .await?;
                        Ok(true)
                    }
                    CreateTaskOutcome::UnsupportedModel(message) => {
                        self.fail(
                            persisted,
                            format!("fallback model also unsupported: {message}"),
                        )
                        .await?;
                        Ok(false)
                    }
                    CreateTaskOutcome::Permanent(message) => {
                        self.fail(
                            persisted,
                            format!("fallback create task rejected: {message}"),
                        )
                        .await?;
                        Ok(false)
                    }
                    CreateTaskOutcome::Transient(message) => {
                        self.record_attempt_model(persisted, &fallback, true)
                            .await?;
                        anyhow::bail!("fallback create task failed transiently: {message}")
                    }
                    CreateTaskOutcome::Ambiguous => {
                        self.mark_ambiguous(persisted, "fallback create task outcome ambiguous")
                            .await?;
                        anyhow::bail!(
                            "fallback create task ambiguous; awaiting reconciliation on restart"
                        )
                    }
                }
            }
        }
    }

    async fn create_task(
        &self,
        owner: &str,
        repo: &str,
        profile: &str,
        model: &str,
        base_ref: &str,
        prompt: &str,
    ) -> CreateTaskOutcome {
        let request = CreateTaskRequest {
            custom_agent: profile.to_string(),
            model: model.to_string(),
            prompt: prompt.to_string(),
            base_ref: base_ref.to_string(),
            create_pull_request: false,
        };
        self.github.create_task(owner, repo, &request).await
    }

    /// Post exactly one `ExecutionStarted` comment.
    ///
    /// Once a write may have been sent, recovery is read-only: repeated
    /// authoritative listings may adopt the landed comment, but never recreate
    /// it after a zero-match read.
    async fn post_execution_started(
        &self,
        run: &RunId,
        execution: &ExecutionId,
        persisted: &mut PersistedExecutionRecord,
    ) -> Result<()> {
        if persisted.record.is_complete() {
            return Ok(());
        }
        let task_id = persisted.record.task_id.clone().ok_or_else(|| {
            anyhow::anyhow!("cannot post ExecutionStarted before the task is confirmed")
        })?;

        let event = WorkGraphEvent::new(
            run.clone(),
            persisted.record.project_item_node_id.clone(),
            persisted.record.subject_node_id.clone(),
            WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                execution_id: execution.clone(),
                task_id,
            }),
        )
        .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
        let summary = summary_for(&event);
        let body = render_comment(&event, &summary).map_err(|error| {
            anyhow::anyhow!("failed to render the ExecutionStarted comment: {error}")
        })?;

        let max_attempts = self.config.comment_api.max_attempts.max(1);
        let backoff = Duration::from_millis(self.config.comment_api.retry_backoff_ms);
        for attempt in 1..=max_attempts {
            let comments = self
                .github
                .list_issue_comments(
                    &persisted.record.repository,
                    persisted.record.subject_number,
                )
                .await
                .context("failed to list issue comments before posting ExecutionStarted")?;
            if let Some(adopted) = self.adopt_own_published_comment(&comments, &event)? {
                let mut updated = persisted.record.clone();
                updated.set_comment(adopted.comment_node_id);
                self.persist(persisted, updated).await?;
                return Ok(());
            }
            if persisted.record.ambiguous {
                if attempt < max_attempts {
                    tokio::time::sleep(backoff).await;
                }
                continue;
            }
            break;
        }

        if persisted.record.ambiguous {
            anyhow::bail!(
                "ambiguous ExecutionStarted publication remains unresolved after {max_attempts} authoritative reads; refusing to recreate"
            );
        }

        // Persist ambiguity before the external write. A crash or lost response
        // therefore resumes with authoritative reads only.
        self.mark_ambiguous(
            persisted,
            "ExecutionStarted publication outcome pending authoritative reconciliation",
        )
        .await?;
        match self
            .github
            .create_issue_comment(
                &persisted.record.repository,
                persisted.record.subject_number,
                &body,
            )
            .await
        {
            Ok(comment) => {
                let mut updated = persisted.record.clone();
                updated.set_comment(comment.node_id);
                self.persist(persisted, updated).await
            }
            Err(error) => Err(error).context(
                "ExecutionStarted publication outcome is ambiguous; awaiting read-only reconciliation",
            ),
        }
    }

    /// The trusted, unedited comments authored by `trusted`.
    fn observed_comments(
        &self,
        comments: &[IssueComment],
        trusted: &drasi_workgraph_common::trust::TrustedAuthor,
    ) -> Vec<ObservedComment> {
        comments
            .iter()
            .filter(|comment| comment.is_authored_by(trusted))
            .filter(|comment| comment.is_unedited())
            .filter_map(|comment| {
                parse_comment(&comment.body)
                    .ok()
                    .map(|parsed| ObservedComment {
                        comment_node_id: comment.node_id.clone(),
                        comment: parsed,
                    })
            })
            .collect()
    }

    /// Adopt **this** reaction's own already-published `intended` event.
    ///
    /// The deterministic `eventId` does not cover the payload, so adoption
    /// requires canonical event JSON — envelope *and* payload — byte-identical
    /// to `intended`. A single divergent comment claiming that event ID, or two
    /// that disagree, fails closed as a hard error rather than being adopted.
    fn adopt_own_published_comment(
        &self,
        comments: &[IssueComment],
        intended: &WorkGraphEvent,
    ) -> Result<Option<ObservedComment>> {
        let observed = self.observed_comments(comments, &self.config.trusted_execution_author());
        let accepted = adopt_published_event(&observed, intended)
            .map_err(|error| anyhow::anyhow!("comment reconciliation failed: {error}"))?;
        Ok(accepted.cloned())
    }

    async fn record_attempt_model(
        &self,
        persisted: &mut PersistedExecutionRecord,
        model: &str,
        used_fallback: bool,
    ) -> Result<()> {
        let mut updated = persisted.record.clone();
        updated.set_attempt_model(model, used_fallback);
        self.persist(persisted, updated).await
    }

    async fn confirm_task(
        &self,
        persisted: &mut PersistedExecutionRecord,
        model: &str,
        used_fallback: bool,
        task_id: String,
        task_url: String,
    ) -> Result<()> {
        let mut updated = persisted.record.clone();
        let url = (!task_url.is_empty()).then_some(task_url);
        updated.set_task(model, used_fallback, task_id, url);
        self.persist(persisted, updated).await
    }

    async fn fail(
        &self,
        persisted: &mut PersistedExecutionRecord,
        message: impl Into<String>,
    ) -> Result<()> {
        let message = message.into();
        warn!("[{}] {message}", self.reaction_name);
        let mut updated = persisted.record.clone();
        updated.set_failed(message);
        self.persist(persisted, updated).await
    }

    async fn mark_ambiguous(
        &self,
        persisted: &mut PersistedExecutionRecord,
        message: impl Into<String>,
    ) -> Result<()> {
        let mut updated = persisted.record.clone();
        updated.set_ambiguous(message);
        self.persist(persisted, updated).await
    }

    /// Compare-and-swap `next` into the store, refreshing the in-memory witness
    /// and emitting the structured execution-state log for terminal/ambiguous
    /// transitions.
    async fn persist(
        &self,
        persisted: &mut PersistedExecutionRecord,
        next: ExecutionRecord,
    ) -> Result<()> {
        let Some(bytes) =
            compare_and_swap_record(self.store.clone(), self.store_id(), &persisted.bytes, &next)
                .await?
        else {
            anyhow::bail!(
                "execution record for run '{}' changed underneath this writer",
                next.run_id
            );
        };
        persisted.record = next;
        persisted.bytes = bytes;
        if let Some(log) =
            WorkGraphExecutionStateV1::from_record(self.reaction_name, &persisted.record)
        {
            warn!(
                target: "workgraph.execution_state",
                "{}",
                serde_json::to_string(&log).unwrap_or_default()
            );
        }
        Ok(())
    }
}
