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

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::CheckpointState;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::{StateStoreCompareAndSwapResult, StateStoreProvider};
use drasi_lib::Reaction;
use log::{error, info, warn};

use crate::candidate::RoutingCandidate;
use crate::config::{WorkgraphRouterReactionConfig, ROUTE_QUERY_ID};
use crate::decision::RoutingDecision;
use crate::github_client::GithubClient;
use crate::reconciliation::reconcile_progress;
use crate::rules::{PolicyMode, RoutingPolicyEngine, RulesV1PolicyEngine};
use crate::state::{
    create_reservation_if_absent, load_reservation_with_bytes, load_routing_state,
    reservation_store_key, save_routing_state, serialize_reservation, PersistedReservationRecord,
    ReservationRecord, RoutingStateRecord, SideEffectProgress,
};
use crate::validation::validate_candidate;
use crate::WorkgraphRouterReactionBuilder;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ReservationFencedError {
    message: String,
}

#[derive(Debug, Clone)]
struct OwnedReservation {
    record: ReservationRecord,
    persisted_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreflightStatus {
    Source,
    Destination,
}

pub struct WorkgraphRouterReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: WorkgraphRouterReactionConfig,
    runner_instance_id: String,
}

impl WorkgraphRouterReaction {
    pub fn builder(id: impl Into<String>) -> WorkgraphRouterReactionBuilder {
        WorkgraphRouterReactionBuilder::new(id)
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: WorkgraphRouterReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
        recovery_policy: Option<ReactionRecoveryPolicy>,
    ) -> Self {
        let runner_instance_id = format!(
            "{id}:{}:{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        if let Some(policy) = recovery_policy {
            params = params.with_recovery_policy(policy);
        }
        Self {
            base: ReactionBase::new(params),
            config,
            runner_instance_id,
        }
    }
}

#[async_trait]
impl Reaction for WorkgraphRouterReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "workgraph-router"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
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

    async fn start(&self) -> anyhow::Result<()> {
        log_component_start("WorkGraph Router Reaction", &self.base.id);
        self.config
            .validate(&self.base.queries, None)
            .context("invalid workgraph-router config")?;

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting workgraph-router reaction".to_string()),
            )
            .await;

        let github = GithubClient::from_config(&self.config)?;
        let shutdown_rx = self.base.create_shutdown_channel().await;
        let reaction_name = self.base.id.clone();
        let base = self.base.clone_shared();
        let config = self.config.clone();
        let runner_instance_id = self.runner_instance_id.clone();

        let handle = tokio::spawn(async move {
            let mut checkpoint_state = CheckpointState::load(&base).await;
            run_processing_loop(
                &reaction_name,
                base,
                config,
                github,
                &runner_instance_id,
                &mut checkpoint_state,
                shutdown_rx,
            )
            .await;
        });

        self.base.set_processing_task(handle).await;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Workgraph router running".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }
}

async fn run_processing_loop(
    reaction_name: &str,
    base: ReactionBase,
    config: WorkgraphRouterReactionConfig,
    github: GithubClient,
    runner_instance_id: &str,
    checkpoint_state: &mut CheckpointState,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        let event = tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            event = base.priority_queue.dequeue() => event,
        };

        if event.query_id != ROUTE_QUERY_ID {
            warn!(
                "[{}] received result for unexpected query '{}'; expected '{}'",
                reaction_name, event.query_id, ROUTE_QUERY_ID
            );
            continue;
        }

        if let Err(error) = process_query_result(
            reaction_name,
            &base,
            &config,
            &github,
            runner_instance_id,
            &event,
        )
        .await
        {
            if error.downcast_ref::<ReservationFencedError>().is_some() {
                info!(
                    "[{}] reservation fenced for query '{}' sequence {}: {}",
                    reaction_name, event.query_id, event.sequence, error
                );
                continue;
            }
            error!(
                "[{}] processing failed for query '{}' sequence {}: {error:#}",
                reaction_name, event.query_id, event.sequence
            );
            base.set_status(
                ComponentStatus::Error,
                Some(format!("Workgraph-router failed: {error:#}")),
            )
            .await;
            if config.strict_recovery {
                return;
            }
            continue;
        }

        if let Err(error) = checkpoint_state
            .advance(&base, &event.query_id, event.sequence)
            .await
        {
            error!(
                "[{}] checkpoint update failed for sequence {}: {error:#}",
                reaction_name, event.sequence
            );
            base.set_status(
                ComponentStatus::Error,
                Some(format!("Workgraph-router checkpoint failure: {error:#}")),
            )
            .await;
            return;
        }
    }
}

async fn process_query_result(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    github: &GithubClient,
    runner_instance_id: &str,
    result: &QueryResult,
) -> anyhow::Result<()> {
    for diff in &result.results {
        match diff {
            ResultDiff::Add { data, .. } => {
                let candidate: RoutingCandidate = serde_json::from_value(data.clone())
                    .context("failed to deserialize added row into RoutingCandidate")?;
                process_candidate(
                    reaction_name,
                    base,
                    config,
                    github,
                    runner_instance_id,
                    &candidate,
                )
                .await?;
            }
            ResultDiff::Update { .. } | ResultDiff::Delete { .. } => {
                info!(
                    "[{}] ignoring non-added diff for query '{}'",
                    reaction_name, result.query_id
                );
            }
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
        }
    }
    Ok(())
}

async fn process_candidate(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    github: &GithubClient,
    runner_instance_id: &str,
    candidate: &RoutingCandidate,
) -> anyhow::Result<()> {
    validate_candidate(candidate, config).context("row validation failed")?;

    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("durable state store is required for workgraph-router"))?;

    let (reservation, mut owned_reservation, mut state) = reserve_or_resume(
        store.clone(),
        &base.id,
        config,
        candidate,
        runner_instance_id,
    )
    .await?;

    if reservation.completed && state.progress.is_complete() {
        info!(
            "[{}] reservation '{}' already completed; skipping",
            reaction_name, reservation.reservation_key
        );
        return Ok(());
    }

    let mut owned_reservation = if let Some(owned) = owned_reservation.take() {
        owned
    } else if reservation.completed {
        // Reservation was completed but persisted state wasn't fully marked complete yet.
        // Stale runners must not mutate this reservation.
        return Ok(());
    } else {
        return Err(ReservationFencedError {
            message: format!(
                "reservation '{}' is fenced by owner '{}' epoch {} and is not complete",
                reservation.reservation_key,
                reservation
                    .owner_instance_id
                    .as_deref()
                    .unwrap_or("unknown"),
                reservation.fencing_epoch
            ),
        }
        .into());
    };

    let reservation_policy_mismatch = reservation.policy_id != config.policy_id
        || reservation.policy_type != config.policy_type
        || reservation.policy_version != config.policy_version;

    let decision = if let Some(decision) = state.decision.clone() {
        if reservation_policy_mismatch {
            info!(
                "[{}] resuming reservation '{}' with persisted decision '{}' bound to policy {}@{}",
                reaction_name,
                reservation.reservation_key,
                decision.decision_id,
                decision.policy_id,
                decision.policy_version
            );
        }
        decision
    } else {
        if reservation_policy_mismatch {
            anyhow::bail!(
                "reservation '{}' is bound to policy {}@{} but has no persisted decision to resume",
                reservation.reservation_key,
                reservation.policy_id,
                reservation.policy_version
            );
        }
        let mode = PolicyMode::try_from(config.policy_type.as_str())
            .context("unable to resolve policyType for routing evaluation")?;
        let engine: Box<dyn RoutingPolicyEngine> = match mode {
            PolicyMode::RulesV1 => Box::<RulesV1PolicyEngine>::default(),
            PolicyMode::Linear | PolicyMode::Llm => {
                anyhow::bail!(
                    "policyType '{}' is declared but not implemented; only rules_v1 is supported",
                    config.policy_type
                )
            }
        };
        let outcome = engine
            .evaluate(candidate)
            .context("rules evaluation rejected candidate")?;
        if !config.allows_transition(&outcome.from_status, &outcome.to_status) {
            anyhow::bail!(
                "selected transition {} -> {} is not allowlisted",
                outcome.from_status,
                outcome.to_status
            );
        }
        let decision = RoutingDecision::from_policy(config, candidate, outcome);
        state.selected_transition =
            Some((decision.from_status.clone(), decision.to_status.clone()));
        state.decision = Some(decision.clone());
        persist_state_with_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            &state,
            "failed to persist routing decision state",
        )
        .await?;
        decision
    };

    let mut progress =
        reconcile_progress(github, candidate, &decision, config, state.progress.clone())
            .await
            .context("failed to reconcile existing side effects")?;
    state.mark_progress(progress.clone());

    if !progress.decision_comment_written {
        renew_reservation_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            "decision comment side effect",
        )
        .await?;
        match run_github_preflight(github, candidate, &decision).await {
            Ok(PreflightStatus::Source) => {}
            Ok(PreflightStatus::Destination) => {
                progress = reconcile_progress(
                    github,
                    candidate,
                    &decision,
                    config,
                    progress.clone(),
                )
                .await
                .context(
                    "failed to reconcile when destination status observed before decision comment",
                )?;
                if !progress.is_complete() {
                    anyhow::bail!(
                        "project item {} already at destination '{}' before decision comment but side effects are incomplete",
                        candidate.project_item_id,
                        decision.to_status
                    );
                }
            }
            Err(error) => {
                state.mark_error_with_epoch(
                    format!("github preflight failed: {error:#}"),
                    false,
                    owned_reservation.record.fencing_epoch,
                );
                persist_state_with_ownership(
                    store.clone(),
                    &base.id,
                    config,
                    runner_instance_id,
                    &mut owned_reservation,
                    &state,
                    "failed to persist preflight failure state",
                )
                .await?;
                return Err(error);
            }
        }

        if !progress.decision_comment_written {
            let decision_body = decision.decision_comment(candidate)?;
            if let Err(error) = github
                .create_issue_comment(
                    &candidate.subject_repo,
                    candidate.subject_issue_number,
                    &decision_body,
                )
                .await
            {
                state.mark_error_with_epoch(
                    format!("decision comment write failed: {error:#}"),
                    true,
                    owned_reservation.record.fencing_epoch,
                );
                persist_state_with_ownership(
                    store.clone(),
                    &base.id,
                    config,
                    runner_instance_id,
                    &mut owned_reservation,
                    &state,
                    "failed to persist ambiguous decision-comment error",
                )
                .await?;
                progress =
                    reconcile_progress(github, candidate, &decision, config, progress.clone())
                        .await
                        .context("failed to reconcile after decision comment error")?;
                if !progress.decision_comment_written {
                    anyhow::bail!("decision comment write failed and could not be reconciled");
                }
            }
            progress.decision_comment_written = true;
        }
        state.mark_progress(progress.clone());
        persist_state_with_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            &state,
            "failed to persist decision comment progress",
        )
        .await?;
    }

    if !progress.responsibility_written {
        renew_reservation_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            "responsibility comment side effect",
        )
        .await?;
        match run_github_preflight(github, candidate, &decision).await {
            Ok(PreflightStatus::Source) => {}
            Ok(PreflightStatus::Destination) => {
                progress = reconcile_progress(github, candidate, &decision, config, progress.clone())
                    .await
                    .context("failed to reconcile when destination status observed before responsibility comment")?;
                if !progress.is_complete() {
                    anyhow::bail!(
                        "project item {} already at destination '{}' before responsibility comment but side effects are incomplete",
                        candidate.project_item_id,
                        decision.to_status
                    );
                }
            }
            Err(error) => {
                state.mark_error_with_epoch(
                    format!("github preflight failed: {error:#}"),
                    false,
                    owned_reservation.record.fencing_epoch,
                );
                persist_state_with_ownership(
                    store.clone(),
                    &base.id,
                    config,
                    runner_instance_id,
                    &mut owned_reservation,
                    &state,
                    "failed to persist preflight failure state",
                )
                .await?;
                return Err(error);
            }
        }

        if !progress.responsibility_written {
            let responsibility_body = decision.responsibility_comment(candidate)?;
            if let Err(error) = github
                .create_issue_comment(
                    &candidate.subject_repo,
                    candidate.subject_issue_number,
                    &responsibility_body,
                )
                .await
            {
                state.mark_error_with_epoch(
                    format!("responsibility write failed: {error:#}"),
                    true,
                    owned_reservation.record.fencing_epoch,
                );
                persist_state_with_ownership(
                    store.clone(),
                    &base.id,
                    config,
                    runner_instance_id,
                    &mut owned_reservation,
                    &state,
                    "failed to persist ambiguous responsibility error",
                )
                .await?;
                progress =
                    reconcile_progress(github, candidate, &decision, config, progress.clone())
                        .await
                        .context("failed to reconcile after responsibility write error")?;
                if !progress.responsibility_written {
                    anyhow::bail!("responsibility write failed and could not be reconciled");
                }
            }
            progress.responsibility_written = true;
        }
        state.mark_progress(progress.clone());
        persist_state_with_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            &state,
            "failed to persist responsibility progress",
        )
        .await?;
    }

    if !progress.project_status_updated {
        renew_reservation_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            "project status side effect",
        )
        .await?;
        match run_github_preflight(github, candidate, &decision).await {
            Ok(PreflightStatus::Source) => {
                if let Err(error) = github
                    .update_project_status(
                        &candidate.project_id,
                        &candidate.project_item_id,
                        &decision.to_status,
                    )
                    .await
                {
                    state.mark_error_with_epoch(
                        format!("project status update failed: {error:#}"),
                        true,
                        owned_reservation.record.fencing_epoch,
                    );
                    persist_state_with_ownership(
                        store.clone(),
                        &base.id,
                        config,
                        runner_instance_id,
                        &mut owned_reservation,
                        &state,
                        "failed to persist ambiguous project-status error",
                    )
                    .await?;
                    progress =
                        reconcile_progress(github, candidate, &decision, config, progress.clone())
                            .await
                            .context("failed to reconcile after project status error")?;
                    if !progress.project_status_updated {
                        anyhow::bail!("project status update failed and could not be reconciled");
                    }
                }
            }
            Ok(PreflightStatus::Destination) => {
                progress =
                    reconcile_progress(github, candidate, &decision, config, progress.clone())
                        .await
                        .context(
                            "failed to reconcile when destination status observed before mutation",
                        )?;
                if !progress.is_complete() {
                    anyhow::bail!(
                        "project item {} status already '{}' but comments are incomplete; refusing to overwrite",
                        candidate.project_item_id,
                        decision.to_status
                    );
                }
                progress.project_status_updated = true;
            }
            Err(error) => {
                state.mark_error_with_epoch(
                    format!("github preflight failed: {error:#}"),
                    false,
                    owned_reservation.record.fencing_epoch,
                );
                persist_state_with_ownership(
                    store.clone(),
                    &base.id,
                    config,
                    runner_instance_id,
                    &mut owned_reservation,
                    &state,
                    "failed to persist preflight failure state",
                )
                .await?;
                return Err(error);
            }
        }
        progress.project_status_updated = true;
        state.mark_progress(progress.clone());
        persist_state_with_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            &state,
            "failed to persist project-status progress",
        )
        .await?;
    }

    state.clear_error();
    state.mark_progress(SideEffectProgress {
        decision_comment_written: true,
        responsibility_written: true,
        project_status_updated: true,
    });
    persist_state_with_ownership(
        store.clone(),
        &base.id,
        config,
        runner_instance_id,
        &mut owned_reservation,
        &state,
        "failed to persist completed routing state",
    )
    .await?;
    complete_reservation(
        store,
        &base.id,
        config,
        runner_instance_id,
        &mut owned_reservation,
        &decision.decision_id,
    )
    .await?;

    Ok(())
}

async fn reserve_or_resume(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    config: &WorkgraphRouterReactionConfig,
    candidate: &RoutingCandidate,
    runner_instance_id: &str,
) -> anyhow::Result<(
    ReservationRecord,
    Option<OwnedReservation>,
    RoutingStateRecord,
)> {
    let reservation_key = candidate.reservation_key();
    loop {
        let persisted = if let Some(existing) =
            load_reservation_with_bytes(store.clone(), store_id, &reservation_key).await?
        {
            existing
        } else {
            let created = ReservationRecord {
                reservation_key: reservation_key.clone(),
                execution_id: candidate.execution_id.clone(),
                required_event_type: candidate.required_event_type.clone(),
                owner_instance_id: Some(runner_instance_id.to_string()),
                fencing_epoch: 1,
                lease_expires_at_unix_secs: reservation_lease_deadline(config),
                policy_id: config.policy_id.clone(),
                policy_type: config.policy_type.clone(),
                policy_version: config.policy_version.clone(),
                decision_id: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                completed: false,
            };
            match create_reservation_if_absent(store.clone(), store_id, &created).await? {
                Some(_) => continue,
                None => PersistedReservationRecord {
                    bytes: serialize_reservation(&created)?,
                    record: created,
                },
            }
        };

        let mut state = if let Some(existing_state) =
            load_routing_state(store.clone(), store_id, &reservation_key).await?
        {
            existing_state
        } else {
            RoutingStateRecord::new(candidate, &persisted.record)
        };

        if persisted.record.owner_instance_id.as_deref() == Some(runner_instance_id) {
            let mut owned = OwnedReservation {
                record: persisted.record.clone(),
                persisted_bytes: persisted.bytes.clone(),
            };
            renew_reservation_ownership(
                store.clone(),
                store_id,
                config,
                runner_instance_id,
                &mut owned,
                "refresh reservation ownership",
            )
            .await?;
            if load_routing_state(store.clone(), store_id, &reservation_key)
                .await?
                .is_none()
            {
                save_routing_state(store.clone(), store_id, &state)
                    .await
                    .context("failed to initialize routing state for owned reservation")?;
            }
            return Ok((owned.record.clone(), Some(owned), state));
        }

        let failed_takeover_ready =
            state.failed && state.failure_fencing_epoch == Some(persisted.record.fencing_epoch);
        if !persisted.record.completed
            && (failed_takeover_ready || reservation_lease_expired(&persisted.record))
        {
            let mut takeover = persisted.record.clone();
            takeover.owner_instance_id = Some(runner_instance_id.to_string());
            takeover.fencing_epoch = takeover.fencing_epoch.max(1).saturating_add(1);
            takeover.lease_expires_at_unix_secs = reservation_lease_deadline(config);
            let key = reservation_store_key(&reservation_key);
            let new_bytes = serialize_reservation(&takeover)?;
            let swapped = store
                .compare_and_swap(
                    store_id,
                    &key,
                    Some(persisted.bytes.as_slice()),
                    new_bytes.clone(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("state-store CAS reservation takeover failed: {e}"))?;
            if matches!(swapped, StateStoreCompareAndSwapResult::Swapped) {
                info!(
                    "[{}] took over reservation '{}' at epoch {}",
                    store_id, reservation_key, takeover.fencing_epoch
                );
                if load_routing_state(store.clone(), store_id, &reservation_key)
                    .await?
                    .is_none()
                {
                    save_routing_state(store.clone(), store_id, &state)
                        .await
                        .context("failed to initialize routing state during takeover")?;
                }
                let owned = OwnedReservation {
                    record: takeover.clone(),
                    persisted_bytes: new_bytes,
                };
                return Ok((takeover, Some(owned), state));
            }
            continue;
        }

        return Ok((persisted.record, None, state));
    }
}

async fn run_github_preflight(
    github: &GithubClient,
    candidate: &RoutingCandidate,
    decision: &RoutingDecision,
) -> anyhow::Result<PreflightStatus> {
    if !github
        .issue_is_open(&candidate.subject_repo, candidate.subject_issue_number)
        .await
        .context("GitHub issue preflight failed")?
    {
        anyhow::bail!(
            "subject issue {}/{} is not open",
            candidate.subject_repo,
            candidate.subject_issue_number
        );
    }
    let current_status = github
        .current_project_status(&candidate.project_id, &candidate.project_item_id)
        .await
        .context("GitHub project-status preflight failed")?;
    if current_status == decision.from_status {
        return Ok(PreflightStatus::Source);
    }
    if current_status == decision.to_status {
        return Ok(PreflightStatus::Destination);
    }
    {
        anyhow::bail!(
            "project item {} status is '{}' (expected '{}' or '{}')",
            candidate.project_item_id,
            current_status,
            decision.from_status,
            decision.to_status
        );
    }
}

fn reservation_lease_deadline(config: &WorkgraphRouterReactionConfig) -> i64 {
    let lease_secs = i64::try_from(config.reservation_lease_secs).unwrap_or(i64::MAX);
    chrono::Utc::now().timestamp().saturating_add(lease_secs)
}

fn reservation_lease_expired(reservation: &ReservationRecord) -> bool {
    reservation.lease_expires_at_unix_secs <= chrono::Utc::now().timestamp()
}

async fn renew_reservation_ownership(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    config: &WorkgraphRouterReactionConfig,
    runner_instance_id: &str,
    owned: &mut OwnedReservation,
    action: &str,
) -> anyhow::Result<()> {
    if owned.record.owner_instance_id.as_deref() != Some(runner_instance_id) {
        return Err(ReservationFencedError {
            message: format!(
                "reservation '{}' ownership changed before {} (owner='{}' epoch={})",
                owned.record.reservation_key,
                action,
                owned
                    .record
                    .owner_instance_id
                    .as_deref()
                    .unwrap_or("unknown"),
                owned.record.fencing_epoch
            ),
        }
        .into());
    }

    let key = reservation_store_key(&owned.record.reservation_key);
    let mut renewed = owned.record.clone();
    renewed.fencing_epoch = renewed.fencing_epoch.max(1);
    renewed.lease_expires_at_unix_secs = reservation_lease_deadline(config);
    let renewed_bytes = serialize_reservation(&renewed)?;
    let swapped = store
        .compare_and_swap(
            store_id,
            &key,
            Some(owned.persisted_bytes.as_slice()),
            renewed_bytes.clone(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("state-store CAS ownership renewal failed: {e}"))?;
    if matches!(swapped, StateStoreCompareAndSwapResult::Swapped) {
        owned.record = renewed;
        owned.persisted_bytes = renewed_bytes;
        return Ok(());
    }

    let current =
        load_reservation_with_bytes(store, store_id, &owned.record.reservation_key).await?;
    let details = if let Some(current) = current {
        format!(
            "current owner='{}' epoch={} completed={}",
            current
                .record
                .owner_instance_id
                .as_deref()
                .unwrap_or("unknown"),
            current.record.fencing_epoch,
            current.record.completed
        )
    } else {
        "reservation removed".to_string()
    };
    Err(ReservationFencedError {
        message: format!(
            "reservation '{}' fenced before {} ({details})",
            owned.record.reservation_key, action
        ),
    }
    .into())
}

async fn persist_state_with_ownership(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    config: &WorkgraphRouterReactionConfig,
    runner_instance_id: &str,
    owned: &mut OwnedReservation,
    state: &RoutingStateRecord,
    error_context: &str,
) -> anyhow::Result<()> {
    renew_reservation_ownership(
        store.clone(),
        store_id,
        config,
        runner_instance_id,
        owned,
        "state update",
    )
    .await?;
    save_routing_state(store, store_id, state)
        .await
        .with_context(|| error_context.to_string())
}

async fn complete_reservation(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    config: &WorkgraphRouterReactionConfig,
    runner_instance_id: &str,
    owned: &mut OwnedReservation,
    decision_id: &str,
) -> anyhow::Result<()> {
    renew_reservation_ownership(
        store.clone(),
        store_id,
        config,
        runner_instance_id,
        owned,
        "reservation completion",
    )
    .await?;

    let key = reservation_store_key(&owned.record.reservation_key);
    let mut completed = owned.record.clone();
    completed.decision_id = Some(decision_id.to_string());
    completed.completed = true;
    completed.lease_expires_at_unix_secs = reservation_lease_deadline(config);
    let completed_bytes = serialize_reservation(&completed)?;
    let swapped = store
        .compare_and_swap(
            store_id,
            &key,
            Some(owned.persisted_bytes.as_slice()),
            completed_bytes.clone(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("state-store CAS reservation completion failed: {e}"))?;
    if !matches!(swapped, StateStoreCompareAndSwapResult::Swapped) {
        return Err(ReservationFencedError {
            message: format!(
                "reservation '{}' fenced before completion",
                owned.record.reservation_key
            ),
        }
        .into());
    }
    owned.record = completed;
    owned.persisted_bytes = completed_bytes;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::state_store::{MemoryStateStoreProvider, StateStoreResult};

    struct DurableMemoryStateStore {
        inner: MemoryStateStoreProvider,
    }

    impl DurableMemoryStateStore {
        fn new() -> Self {
            Self {
                inner: MemoryStateStoreProvider::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl StateStoreProvider for DurableMemoryStateStore {
        async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
            self.inner.get(store_id, key).await
        }

        async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
            self.inner.set(store_id, key, value).await
        }

        async fn compare_and_swap(
            &self,
            store_id: &str,
            key: &str,
            expected: Option<&[u8]>,
            new_value: Vec<u8>,
        ) -> StateStoreResult<StateStoreCompareAndSwapResult> {
            self.inner
                .compare_and_swap(store_id, key, expected, new_value)
                .await
        }

        async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
            self.inner.delete(store_id, key).await
        }

        async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
            self.inner.contains_key(store_id, key).await
        }

        async fn get_many(
            &self,
            store_id: &str,
            keys: &[&str],
        ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
            self.inner.get_many(store_id, keys).await
        }

        async fn set_many(
            &self,
            store_id: &str,
            entries: &[(&str, &[u8])],
        ) -> StateStoreResult<()> {
            self.inner.set_many(store_id, entries).await
        }

        async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
            self.inner.delete_many(store_id, keys).await
        }

        async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
            self.inner.clear_store(store_id).await
        }

        async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
            self.inner.list_keys(store_id).await
        }

        async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
            self.inner.store_exists(store_id).await
        }

        async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
            self.inner.key_count(store_id).await
        }

        fn is_durable(&self) -> bool {
            true
        }
    }

    fn sample_config() -> WorkgraphRouterReactionConfig {
        WorkgraphRouterReactionConfig {
            policy_id: "policy-1".to_string(),
            policy_type: "rules_v1".to_string(),
            policy_version: "1.0.0".to_string(),
            allowed_projects: vec!["PVT_project".to_string()],
            allowed_repos: vec!["drasi-project/drasi-core".to_string()],
            allowed_event_types: vec!["CompletedIssueValidation".to_string()],
            allowed_status_transitions: vec![
                crate::config::StatusTransition {
                    from: "AwaitingRouting".to_string(),
                    to: "AwaitingIssueRiskProfiling".to_string(),
                },
                crate::config::StatusTransition {
                    from: "AwaitingRouting".to_string(),
                    to: "NeedsMoreInformation".to_string(),
                },
            ],
            allowed_responsibility_types: vec![
                "issue-validation".to_string(),
                "issue-risk-profiling".to_string(),
                "issue-correction".to_string(),
            ],
            allowed_actors: vec!["bot-user".to_string(), "submitter-user".to_string()],
            trusted_routing_authors: vec!["router-user".to_string()],
            trusted_launcher_authors: vec!["launcher-user".to_string()],
            trusted_agent_authors: vec!["agent-user".to_string()],
            trusted_router_authors: vec!["router-user".to_string()],
            timeout_secs: 5,
            reservation_lease_secs: 5,
            ..WorkgraphRouterReactionConfig::default()
        }
    }

    fn sample_candidate() -> RoutingCandidate {
        RoutingCandidate {
            execution_id: "exec-1".to_string(),
            required_event_type: "CompletedIssueValidation".to_string(),
            event_id: "event-1".to_string(),
            event_type: "CompletedIssueValidation".to_string(),
            outcome: "passed".to_string(),
            subject_repo: "drasi-project/drasi-core".to_string(),
            subject_issue_number: 42,
            project_id: "PVT_project".to_string(),
            project_item_id: "PVTI_item".to_string(),
            project_status: "AwaitingRouting".to_string(),
            route_id: "route-1".to_string(),
            route_expected_event_id: "event-1".to_string(),
            route_expected_event_type: "CompletedIssueValidation".to_string(),
            route_expected_subject_repo: "drasi-project/drasi-core".to_string(),
            route_expected_subject_issue_number: 42,
            route_content_version: "sha256:abc".to_string(),
            route_content_profile: "phase2".to_string(),
            responsibility_id: "resp-1".to_string(),
            responsibility_type: "issue-validation".to_string(),
            responsibility_actor: "bot-user".to_string(),
            submitter_actor: "submitter-user".to_string(),
            launcher_author: "launcher-user".to_string(),
            agent_author: "agent-user".to_string(),
            router_author: "router-user".to_string(),
            routing_author: "router-user".to_string(),
            observed_authors: vec![
                "launcher-user".to_string(),
                "agent-user".to_string(),
                "router-user".to_string(),
            ],
            comment_id: 1,
            comment_author: "router-user".to_string(),
            comment_body: "{\"ok\":true}".to_string(),
            comment_edited: false,
            comment_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            comment_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            comment_provenance_event_id: "event-1".to_string(),
            comment_provenance_event_type: "CompletedIssueValidation".to_string(),
            content_version: "sha256:abc".to_string(),
            content_profile: "phase2".to_string(),
        }
    }

    fn make_reaction(id: &str, config: &WorkgraphRouterReactionConfig) -> WorkgraphRouterReaction {
        WorkgraphRouterReaction::builder(id)
            .with_query(ROUTE_QUERY_ID)
            .with_config(config.clone())
            .build()
            .expect("reaction build")
    }

    #[tokio::test]
    async fn simultaneous_initial_claim_allows_single_owner() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let candidate = sample_candidate();
        let reaction_a = make_reaction("router-a", &config);
        let reaction_b = make_reaction("router-b", &config);

        let (a, b) = tokio::join!(
            reserve_or_resume(
                store.clone(),
                "test-store",
                &config,
                &candidate,
                &reaction_a.runner_instance_id
            ),
            reserve_or_resume(
                store.clone(),
                "test-store",
                &config,
                &candidate,
                &reaction_b.runner_instance_id
            )
        );

        let (_, owner_a, _) = a.expect("reserve a");
        let (_, owner_b, _) = b.expect("reserve b");
        assert_ne!(owner_a.is_some(), owner_b.is_some());
    }

    #[tokio::test]
    async fn concurrent_failed_takeover_is_single_winner_and_epoch_bumps_once() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let candidate = sample_candidate();
        let reservation_key = candidate.reservation_key();
        let seeded = ReservationRecord {
            reservation_key: reservation_key.clone(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            owner_instance_id: Some("legacy-owner".to_string()),
            fencing_epoch: 10,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() - 30,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        crate::state::save_reservation(store.clone(), "test-store", &seeded)
            .await
            .expect("seed reservation");
        let mut seeded_state = RoutingStateRecord::new(&candidate, &seeded);
        seeded_state.mark_error("seeded failure", true);
        save_routing_state(store.clone(), "test-store", &seeded_state)
            .await
            .expect("seed state");

        let reaction_a = make_reaction("router-a", &config);
        let reaction_b = make_reaction("router-b", &config);
        let (a, b) = tokio::join!(
            reserve_or_resume(
                store.clone(),
                "test-store",
                &config,
                &candidate,
                &reaction_a.runner_instance_id
            ),
            reserve_or_resume(
                store.clone(),
                "test-store",
                &config,
                &candidate,
                &reaction_b.runner_instance_id
            )
        );
        let (_, owner_a, _) = a.expect("reserve a");
        let (_, owner_b, _) = b.expect("reserve b");
        assert_ne!(owner_a.is_some(), owner_b.is_some());

        let persisted = load_reservation_with_bytes(store, "test-store", &reservation_key)
            .await
            .expect("load reservation")
            .expect("reservation exists");
        assert_eq!(persisted.record.fencing_epoch, 11);
    }

    #[tokio::test]
    async fn stale_owner_is_fenced_after_takeover() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let candidate = sample_candidate();
        let reaction_a = make_reaction("router-a", &config);
        let reaction_b = make_reaction("router-b", &config);

        let (_, owner_a, mut state_a) = reserve_or_resume(
            store.clone(),
            "test-store",
            &config,
            &candidate,
            &reaction_a.runner_instance_id,
        )
        .await
        .expect("owner a reserve");
        let mut owner_a = owner_a.expect("owner a owns reservation");

        state_a.mark_error("seeded failure", true);
        save_routing_state(store.clone(), "test-store", &state_a)
            .await
            .expect("seed failure state");
        owner_a.record.lease_expires_at_unix_secs = chrono::Utc::now().timestamp() - 30;
        crate::state::save_reservation(store.clone(), "test-store", &owner_a.record)
            .await
            .expect("expire owner a lease");
        owner_a.persisted_bytes =
            serialize_reservation(&owner_a.record).expect("serialize owner a");

        let (_, owner_b, _) = reserve_or_resume(
            store.clone(),
            "test-store",
            &config,
            &candidate,
            &reaction_b.runner_instance_id,
        )
        .await
        .expect("owner b takeover");
        assert!(owner_b.is_some());

        let err = renew_reservation_ownership(
            store,
            "test-store",
            &config,
            &reaction_a.runner_instance_id,
            &mut owner_a,
            "stale owner check",
        )
        .await
        .expect_err("stale owner must be fenced");
        assert!(err.downcast_ref::<ReservationFencedError>().is_some());
    }

    #[tokio::test]
    async fn expired_dead_owner_reservation_is_reclaimed_with_epoch_increment() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let candidate = sample_candidate();
        let reservation_key = candidate.reservation_key();
        let seeded = ReservationRecord {
            reservation_key: reservation_key.clone(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            owner_instance_id: Some("dead-owner".to_string()),
            fencing_epoch: 7,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() - 30,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: Some("persisted-decision".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        crate::state::save_reservation(store.clone(), "test-store", &seeded)
            .await
            .expect("seed reservation");
        let mut seeded_state = RoutingStateRecord::new(&candidate, &seeded);
        seeded_state.progress = SideEffectProgress {
            decision_comment_written: true,
            responsibility_written: false,
            project_status_updated: false,
        };
        save_routing_state(store.clone(), "test-store", &seeded_state)
            .await
            .expect("seed state");

        let reaction = make_reaction("router-live", &config);
        let (reservation, owner, state) = reserve_or_resume(
            store.clone(),
            "test-store",
            &config,
            &candidate,
            &reaction.runner_instance_id,
        )
        .await
        .expect("reclaim expired reservation");

        assert!(owner.is_some());
        assert_eq!(reservation.fencing_epoch, 8);
        assert!(state.progress.decision_comment_written);
        assert!(!state.progress.responsibility_written);
    }
}
