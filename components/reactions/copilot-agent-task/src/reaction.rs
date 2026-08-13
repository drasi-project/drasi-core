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

//! The Copilot Agent Task reaction: subscribes to a single launch query and,
//! for every `Add` row, validates it, runs preflight checks, durably
//! reserves the attempt, launches a GitHub Copilot coding-agent task, and
//! posts a `workgraph.execution/v1` issue comment.
//!
//! `Update` and `Delete` diffs (and `Aggregation`/`Noop`) are never acted on
//! — this reaction only launches on rows newly added to the query's result
//! set.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info, warn};

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::CheckpointState;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Reaction;

use crate::config::CopilotAgentTaskReactionConfig;
use crate::github::{
    CreateTaskOutcome, CreateTaskRequest, GitHubClient, GitHubConfig, PreflightError,
    ReconciliationOutcome,
};
use crate::ids::execution_id;
use crate::prompt::{build_prompt, WorkGraphExecutionCommentV1};
use crate::redact::preview;
use crate::row::{validate_row, LaunchRow};
use crate::state::{reserve_or_resume, save, ExecutionRecord, ExecutionStatus, ReservationOutcome};
use crate::CopilotAgentTaskReactionBuilder;

/// The single launch-attempt number this reaction version always uses.
/// Reserved as an explicit field in [`crate::state::ExecutionRecord`] (rather
/// than hardcoding `1` at every call site) so a future version can extend to
/// multi-attempt retries without a storage-format migration.
pub const ATTEMPT: u32 = 1;

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
        // Always Strict: see `default_recovery_policy` below and
        // `CopilotAgentTaskReactionConfig::strict_recovery`.
        params = params.with_recovery_policy(ReactionRecoveryPolicy::Strict);
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
        if self.base.state_store().await.is_none() {
            let msg =
                "Copilot Agent Task reaction requires a durable state store (none configured)";
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

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting Copilot Agent Task reaction".to_string()),
            )
            .await;

        let shutdown_rx = self.base.create_shutdown_channel().await;
        let reaction_name = self.base.id.clone();
        let checkpoint_state = CheckpointState::load(&self.base).await;
        let base = self.base.clone_shared();
        let config = self.config.clone();

        let handle = tokio::spawn(run_loop(
            reaction_name,
            base,
            config,
            Arc::new(client),
            shutdown_rx,
            checkpoint_state,
        ));

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

    async fn enqueue_query_result(&self, result: drasi_lib::channels::QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }

    /// Reservation/execution state must survive restarts for idempotency.
    fn is_durable(&self) -> bool {
        true
    }

    /// A trigger reaction: launching on the entire historical result set on
    /// a fresh start would fire duplicate task creation for already-handled
    /// rows. Only new `Add` rows observed after the reaction starts matter.
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    /// Always Strict: an ambiguous or failed launch must stop the pipeline
    /// for reconciliation rather than silently skip or reset. See
    /// `CopilotAgentTaskReactionConfig::strict_recovery`.
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }
}

/// The reaction's processing loop: dequeue `QueryResult`s, process each
/// `Add` diff, and advance the checkpoint only once every diff in the batch
/// has been durably handled (launched, permanently rejected, or already
/// done).
async fn run_loop(
    reaction_name: String,
    base: ReactionBase,
    config: CopilotAgentTaskReactionConfig,
    client: Arc<GitHubClient>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    mut checkpoints: CheckpointState,
) {
    let status_handle = base.status_handle();

    loop {
        let query_result_arc = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                info!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                break;
            }
            result = base.priority_queue.dequeue() => result,
        };
        let query_result: &QueryResult = query_result_arc.as_ref();

        if query_result.results.is_empty() {
            continue;
        }

        let query_id = &query_result.query_id;
        let seq = query_result.sequence;
        let store = base.state_store().await;
        let Some(store) = store else {
            // start() already requires a store; this should be unreachable,
            // but fail safe rather than panic if it somehow disappears.
            error!("[{reaction_name}] State store unavailable mid-run; stopping");
            status_handle
                .set_status(
                    ComponentStatus::Error,
                    Some("State store unavailable".to_string()),
                )
                .await;
            return;
        };

        let mut transient_failure: Option<String> = None;
        for diff in &query_result.results {
            let ResultDiff::Add { data, .. } = diff else {
                continue;
            };
            match process_add_row(
                &reaction_name,
                &base,
                &config,
                &client,
                store.as_ref(),
                data,
            )
            .await
            {
                Ok(()) => {}
                Err(reason) => {
                    error!(
                        "[{reaction_name}] Transient failure processing row from query '{query_id}' (seq {seq}): {reason}"
                    );
                    transient_failure = Some(reason);
                    break;
                }
            }
        }

        if let Some(reason) = transient_failure {
            warn!(
                "[{reaction_name}] Stopping per Strict recovery policy after transient failure on query '{query_id}' (seq {seq}); this batch will replay from the outbox on restart"
            );
            status_handle
                .set_status(
                    ComponentStatus::Error,
                    Some(format!(
                        "Transient failure on query '{query_id}' (seq {seq}): {reason}"
                    )),
                )
                .await;
            return;
        }

        if let Err(e) = checkpoints.advance(&base, query_id, seq).await {
            error!("[{reaction_name}] Failed to write checkpoint for query '{query_id}' (seq {seq}): {e}");
            status_handle
                .set_status(
                    ComponentStatus::Error,
                    Some(format!(
                        "Checkpoint write failed for query '{query_id}' (seq {seq}): {e}"
                    )),
                )
                .await;
            return;
        }
    }

    info!("[{reaction_name}] Copilot Agent Task processing loop stopped");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("Copilot Agent Task reaction processing task stopped".to_string()),
        )
        .await;
}

/// Process one `Add` row end to end. Returns `Ok(())` when the row is fully
/// handled — launched, already done, or **permanently** rejected (validation
/// or preflight failure, recorded and skipped) — and `Err(reason)` only for
/// a **transient** condition, which must stop the batch (and therefore the
/// checkpoint advance) so it is retried after restart.
async fn process_add_row(
    reaction_name: &str,
    base: &ReactionBase,
    config: &CopilotAgentTaskReactionConfig,
    client: &GitHubClient,
    store: &dyn StateStoreProvider,
    data: &serde_json::Value,
) -> Result<(), String> {
    let store_id = &base.id;

    let row = match LaunchRow::from_json(data) {
        Ok(row) => row,
        Err(e) => {
            warn!("[{reaction_name}] Skipping malformed launch row: {e:#}");
            return Ok(());
        }
    };

    if let Err(e) = validate_row(
        &row,
        &config.allowed_repositories,
        &config.allowed_profiles,
        &config.allowed_models,
    ) {
        warn!(
            "[{reaction_name}] Rejecting row for {} issue #{} (route={}, responsibility={}): {e}",
            row.repository, row.issue_number, row.route_id, row.responsibility_id
        );
        record_permanent_failure(store, store_id, &row, e.to_string()).await;
        return Ok(());
    }

    let exec_id = execution_id(store_id, &row.route_id, &row.responsibility_id, ATTEMPT);

    let reservation = reserve_or_resume(
        store,
        store_id,
        &row.route_id,
        &row.responsibility_id,
        ATTEMPT,
        &exec_id,
        &row.expected_event_id,
        &row.required_event_type,
        &row.repository,
        row.issue_number,
        &row.requested_model,
        row.fallback_model.as_deref(),
    )
    .await
    .map_err(|e| format!("failed to reserve execution state: {e:#}"))?;

    let mut record = match reservation {
        ReservationOutcome::AlreadyDone(_) => {
            info!(
                "[{reaction_name}] Row for route={} responsibility={} already fully processed (duplicate delivery) — skipping",
                row.route_id, row.responsibility_id
            );
            return Ok(());
        }
        ReservationOutcome::PermanentlyFailed(_) => {
            info!(
                "[{reaction_name}] Row for route={} responsibility={} previously permanently failed — skipping",
                row.route_id, row.responsibility_id
            );
            return Ok(());
        }
        ReservationOutcome::New(r) => r,
        ReservationOutcome::ResumeCommentOnly(r) => {
            return resume_comment_only(reaction_name, config, client, store, store_id, &row, r)
                .await;
        }
        ReservationOutcome::NeedsReconciliation(r) => {
            return reconcile_and_resume(reaction_name, config, client, store, store_id, &row, r)
                .await;
        }
    };

    // ---- Preflight ----
    match crate::github::run_preflight(client, &row).await {
        Ok(()) => {}
        Err(PreflightError::Permanent(reason)) => {
            warn!(
                "[{reaction_name}] Preflight rejected row for {} issue #{}: {reason}",
                row.repository, row.issue_number
            );
            record.status = ExecutionStatus::Failed;
            record.last_error = Some(reason);
            record.touch();
            let _ = save(store, store_id, &record).await;
            return Ok(());
        }
        Err(PreflightError::Transient(reason)) => {
            return Err(format!("preflight transient failure: {reason}"));
        }
    }

    // ---- Mark Starting (durable, before the external call) ----
    record.status = ExecutionStatus::Starting;
    record.touch();
    save(store, store_id, &record)
        .await
        .map_err(|e| format!("failed to persist Starting state: {e:#}"))?;

    launch_and_comment(reaction_name, config, client, store, store_id, &row, record).await
}

/// Record a permanent (fail-closed) validation rejection, best-effort. If we
/// cannot even parse `route_id`/`responsibility_id` this silently no-ops —
/// there is no reservation key to record against, and the row was already
/// logged by the caller.
async fn record_permanent_failure(
    store: &dyn StateStoreProvider,
    store_id: &str,
    row: &LaunchRow,
    reason: String,
) {
    let exec_id = execution_id(store_id, &row.route_id, &row.responsibility_id, ATTEMPT);
    let mut record = ExecutionRecord::new_reserved(
        &row.route_id,
        &row.responsibility_id,
        ATTEMPT,
        &exec_id,
        &row.expected_event_id,
        &row.required_event_type,
        &row.repository,
        row.issue_number,
        &row.requested_model,
        row.fallback_model.as_deref(),
    );
    record.status = ExecutionStatus::Failed;
    record.last_error = Some(reason);
    record.touch();
    if let Err(e) = save(store, store_id, &record).await {
        warn!(
            "failed to persist permanent-failure record for route={} responsibility={}: {e:#}",
            row.route_id, row.responsibility_id
        );
    }
}

/// Resume a launch attempt that crashed (or was ambiguous) between
/// reservation and confirmed task creation: run reconciliation before doing
/// anything else. Never blindly retries creation.
async fn reconcile_and_resume(
    reaction_name: &str,
    config: &CopilotAgentTaskReactionConfig,
    client: &GitHubClient,
    store: &dyn StateStoreProvider,
    store_id: &str,
    row: &LaunchRow,
    mut record: ExecutionRecord,
) -> Result<(), String> {
    let (owner, repo) = row
        .owner_and_repo()
        .map_err(|e| format!("cannot reconcile: {e:#}"))?;

    info!(
        "[{reaction_name}] Reconciling ambiguous/interrupted launch for route={} responsibility={} (executionId={})",
        row.route_id, row.responsibility_id, record.execution_id
    );

    let outcome = client
        .reconcile(owner, repo, &record.execution_id)
        .await
        .map_err(|e| format!("reconciliation lookup failed: {e}"))?;

    match outcome {
        ReconciliationOutcome::NoMatch => {
            // No task exists under this executionId: safe to (re-)attempt a
            // fresh launch from preflight onward.
            info!(
                "[{reaction_name}] Reconciliation found no existing task for executionId={} — retrying launch",
                record.execution_id
            );
            record.status = ExecutionStatus::Reserved;
            record.touch();
            save(store, store_id, &record)
                .await
                .map_err(|e| format!("failed to persist reservation reset: {e:#}"))?;

            match crate::github::run_preflight(client, row).await {
                Ok(()) => {}
                Err(PreflightError::Permanent(reason)) => {
                    record.status = ExecutionStatus::Failed;
                    record.last_error = Some(reason);
                    record.touch();
                    let _ = save(store, store_id, &record).await;
                    return Ok(());
                }
                Err(PreflightError::Transient(reason)) => {
                    return Err(format!("preflight transient failure: {reason}"));
                }
            }
            record.status = ExecutionStatus::Starting;
            record.touch();
            save(store, store_id, &record)
                .await
                .map_err(|e| format!("failed to persist Starting state: {e:#}"))?;
            launch_and_comment(reaction_name, config, client, store, store_id, row, record).await
        }
        ReconciliationOutcome::ExactMatch(task) => {
            info!(
                "[{reaction_name}] Reconciliation adopted existing task {} for executionId={}",
                task.id, record.execution_id
            );
            record.status = ExecutionStatus::Started;
            record.task_id = Some(task.id);
            record.task_url = Some(task.url);
            // These may already be set from a prior attempt that recorded
            // the model/fallback choice before the outcome became ambiguous
            // (see the `Ambiguous` arm of `launch_and_comment`); only fill
            // them in if genuinely absent, and never guess a request time
            // that was never observed — `now()` is the best available
            // approximation for an adopted, previously-unconfirmed task.
            record
                .model_used
                .get_or_insert_with(|| record.requested_model.clone());
            record.request_time.get_or_insert_with(crate::github::now);
            record.touch();
            save(store, store_id, &record)
                .await
                .map_err(|e| format!("failed to persist adopted task: {e:#}"))?;
            post_comment_and_finish(reaction_name, config, client, store, store_id, row, record)
                .await
        }
        ReconciliationOutcome::Ambiguous(matches) => {
            warn!(
                "[{reaction_name}] Reconciliation found {} candidate tasks for executionId={} — staying Ambiguous, will not guess",
                matches.len(),
                record.execution_id
            );
            record.status = ExecutionStatus::Ambiguous;
            record.last_error = Some(format!(
                "{} candidate tasks matched executionId; manual reconciliation required",
                matches.len()
            ));
            record.touch();
            let _ = save(store, store_id, &record).await;
            Err(format!(
                "launch for route={} responsibility={} remains ambiguous ({} candidates)",
                row.route_id,
                row.responsibility_id,
                matches.len()
            ))
        }
    }
}

/// Resume an attempt whose task was already confirmed created but whose
/// comment was not yet recorded as posted.
async fn resume_comment_only(
    reaction_name: &str,
    config: &CopilotAgentTaskReactionConfig,
    client: &GitHubClient,
    store: &dyn StateStoreProvider,
    store_id: &str,
    row: &LaunchRow,
    record: ExecutionRecord,
) -> Result<(), String> {
    info!(
        "[{reaction_name}] Resuming comment-only step for route={} responsibility={} (task already created: {:?})",
        row.route_id, row.responsibility_id, record.task_id
    );
    post_comment_and_finish(reaction_name, config, client, store, store_id, row, record).await
}

/// Call `create_task` (with the single permitted fallback-model retry),
/// persist the outcome, and — on success — post the workgraph execution
/// comment.
async fn launch_and_comment(
    reaction_name: &str,
    config: &CopilotAgentTaskReactionConfig,
    client: &GitHubClient,
    store: &dyn StateStoreProvider,
    store_id: &str,
    row: &LaunchRow,
    mut record: ExecutionRecord,
) -> Result<(), String> {
    let (owner, repo) = row
        .owner_and_repo()
        .map_err(|e| format!("cannot launch: {e:#}"))?;

    let prompt = build_prompt(row, &record.execution_id);
    info!(
        "[{reaction_name}] Launching task for {}#{} (route={}, responsibility={}, executionId={}) — prompt preview: {}",
        row.repository, row.issue_number, row.route_id, row.responsibility_id, record.execution_id, preview(&prompt)
    );

    let request_time = crate::github::now();
    let mut model_used = row.requested_model.clone();
    let mut used_fallback = false;

    let request = CreateTaskRequest {
        custom_agent: row.agent_profile.clone(),
        model: model_used.clone(),
        prompt: prompt.clone(),
        base_ref: row.base_ref.clone(),
        create_pull_request: false,
    };
    let mut outcome = client.create_task(owner, repo, &request).await;

    if let CreateTaskOutcome::UnsupportedModel(reason) = &outcome {
        if let Some(fallback) = row
            .fallback_model
            .as_deref()
            .filter(|f| !f.is_empty() && *f != row.requested_model)
        {
            warn!(
                "[{reaction_name}] Requested model '{}' unsupported ({reason}) — falling back to '{fallback}' exactly once",
                row.requested_model
            );
            model_used = fallback.to_string();
            used_fallback = true;
            let fallback_request = CreateTaskRequest {
                custom_agent: row.agent_profile.clone(),
                model: model_used.clone(),
                prompt: prompt.clone(),
                base_ref: row.base_ref.clone(),
                create_pull_request: false,
            };
            outcome = client.create_task(owner, repo, &fallback_request).await;
        } else {
            info!(
                "[{reaction_name}] Requested model '{}' unsupported and no usable fallback configured — permanent failure",
                row.requested_model
            );
        }
    }

    match outcome {
        CreateTaskOutcome::Created { id, url } => {
            info!(
                "[{reaction_name}] Task created: id={id} url={url} model={model_used} fallbackUsed={used_fallback}"
            );
            record.status = ExecutionStatus::Started;
            record.model_used = Some(model_used);
            record.used_fallback = used_fallback;
            record.task_id = Some(id);
            record.task_url = Some(url);
            record.request_time = Some(request_time);
            record.last_error = None;
            record.touch();
            save(store, store_id, &record)
                .await
                .map_err(|e| format!("failed to persist Started state: {e:#}"))?;
            post_comment_and_finish(reaction_name, config, client, store, store_id, row, record)
                .await
        }
        CreateTaskOutcome::UnsupportedModel(reason) => {
            record.status = ExecutionStatus::Failed;
            record.last_error = Some(format!("unsupported model (no usable fallback): {reason}"));
            record.touch();
            let _ = save(store, store_id, &record).await;
            Ok(())
        }
        CreateTaskOutcome::Permanent(reason) => {
            warn!("[{reaction_name}] create_task permanently failed: {reason}");
            record.status = ExecutionStatus::Failed;
            record.last_error = Some(reason);
            record.touch();
            let _ = save(store, store_id, &record).await;
            Ok(())
        }
        CreateTaskOutcome::Transient(reason) => {
            record.last_error = Some(reason.clone());
            record.touch();
            let _ = save(store, store_id, &record).await;
            Err(format!("create_task transient failure: {reason}"))
        }
        CreateTaskOutcome::Ambiguous => {
            warn!(
                "[{reaction_name}] create_task outcome ambiguous for route={} responsibility={} (model={model_used}, usedFallback={used_fallback}) — marking Ambiguous, will reconcile before any retry",
                row.route_id, row.responsibility_id
            );
            record.status = ExecutionStatus::Ambiguous;
            // Persist which model this (unconfirmed) attempt used so that if
            // reconciliation later adopts a matching task, the workgraph
            // execution comment reports the model that was actually
            // requested for the call whose outcome went unknown — not the
            // default `requestedModel` reconciliation would otherwise assume.
            record.model_used = Some(model_used);
            record.used_fallback = used_fallback;
            record.last_error = Some("create_task transport error; outcome unknown".to_string());
            record.touch();
            let _ = save(store, store_id, &record).await;
            Err("create_task outcome ambiguous; requires reconciliation".to_string())
        }
    }
}

/// Post the single `workgraph.execution/v1` comment (if enabled and not
/// already posted) and mark the record fully done. This is the last step
/// before the checkpoint is allowed to advance — "checkpoint only after
/// durable task/execution state and comment recorded".
async fn post_comment_and_finish(
    reaction_name: &str,
    config: &CopilotAgentTaskReactionConfig,
    client: &GitHubClient,
    store: &dyn StateStoreProvider,
    store_id: &str,
    row: &LaunchRow,
    mut record: ExecutionRecord,
) -> Result<(), String> {
    if !config.comment_api.enabled {
        record.comment_posted = true; // nothing to do; treat as satisfied.
        record.touch();
        save(store, store_id, &record)
            .await
            .map_err(|e| format!("failed to persist final state: {e:#}"))?;
        return Ok(());
    }

    if record.comment_posted {
        return Ok(());
    }

    let (task_id, task_url, model_used, request_time) = match (
        &record.task_id,
        &record.task_url,
        &record.model_used,
        &record.request_time,
    ) {
        (Some(id), Some(url), Some(model), Some(t)) => (id.clone(), url.clone(), model.clone(), *t),
        _ => {
            return Err(
                "cannot post workgraph execution comment: task details missing from record"
                    .to_string(),
            )
        }
    };

    let envelope = WorkGraphExecutionCommentV1::new(
        row,
        &record.execution_id,
        &task_id,
        &task_url,
        &model_used,
        record.used_fallback,
        &request_time,
    );
    let body = envelope.to_comment_body();

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match client.post_issue_comment(&row.issue_node_id, &body).await {
            Ok(()) => {
                info!(
                    "[{reaction_name}] Posted workgraph.execution/v1 comment on issue {} (executionId={})",
                    row.issue_node_id, record.execution_id
                );
                record.comment_posted = true;
                record.last_error = None;
                record.touch();
                return save(store, store_id, &record)
                    .await
                    .map_err(|e| format!("failed to persist comment_posted state: {e:#}"));
            }
            Err(e) => {
                warn!(
                    "[{reaction_name}] Attempt {attempt}/{} to post workgraph comment failed: {e}",
                    config.comment_api.max_attempts
                );
                record.last_error = Some(e.to_string());
                record.touch();
                let _ = save(store, store_id, &record).await;
                if attempt >= config.comment_api.max_attempts {
                    // GraphQL errors (including HTTP 200 + errors) are
                    // treated as failure per the reaction's contract; the
                    // task itself is never recreated, only the comment step
                    // is retried on the next start.
                    return Err(format!(
                        "failed to post workgraph execution comment after {attempt} attempt(s): {e}"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(config.comment_api.retry_backoff_ms))
                    .await;
            }
        }
    }
}
