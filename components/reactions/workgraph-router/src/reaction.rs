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
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Reaction;
use log::{error, info, warn};
use tokio::sync::Mutex;

use crate::candidate::RoutingCandidate;
use crate::config::{WorkgraphRouterReactionConfig, ROUTE_QUERY_ID};
use crate::decision::RoutingDecision;
use crate::github_client::GithubClient;
use crate::reconciliation::reconcile_progress;
use crate::rules::{PolicyMode, RoutingPolicyEngine, RulesV1PolicyEngine};
use crate::state::{
    load_reservation, load_routing_state, save_reservation, save_routing_state, ReservationRecord,
    RoutingStateRecord, SideEffectProgress,
};
use crate::validation::validate_candidate;
use crate::WorkgraphRouterReactionBuilder;

pub struct WorkgraphRouterReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: WorkgraphRouterReactionConfig,
    reservation_lock: Arc<Mutex<()>>,
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
            reservation_lock: Arc::new(Mutex::new(())),
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
        let reservation_lock = Arc::clone(&self.reservation_lock);

        let handle = tokio::spawn(async move {
            let mut checkpoint_state = CheckpointState::load(&base).await;
            run_processing_loop(
                &reaction_name,
                base,
                config,
                github,
                reservation_lock,
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
    reservation_lock: Arc<Mutex<()>>,
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
            Arc::clone(&reservation_lock),
            &event,
        )
        .await
        {
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
    reservation_lock: Arc<Mutex<()>>,
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
                    Arc::clone(&reservation_lock),
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
    reservation_lock: Arc<Mutex<()>>,
    candidate: &RoutingCandidate,
) -> anyhow::Result<()> {
    validate_candidate(candidate, config).context("row validation failed")?;

    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("durable state store is required for workgraph-router"))?;

    let (mut reservation, mut state) =
        reserve_or_resume(store.clone(), &base.id, config, candidate, reservation_lock).await?;

    if reservation.completed && state.progress.is_complete() {
        info!(
            "[{}] reservation '{}' already completed; skipping",
            reaction_name, reservation.reservation_key
        );
        return Ok(());
    }

    if reservation.policy_id != config.policy_id
        || reservation.policy_version != config.policy_version
    {
        info!(
            "[{}] reservation '{}' already bound to policy {}@{}; skipping newer policy {}@{}",
            reaction_name,
            reservation.reservation_key,
            reservation.policy_id,
            reservation.policy_version,
            config.policy_id,
            config.policy_version
        );
        return Ok(());
    }

    let requires_fresh_preflight = state.decision.is_none();
    if requires_fresh_preflight {
        run_github_preflight(github, candidate).await?;
    }

    let decision = if let Some(decision) = state.decision.clone() {
        decision
    } else {
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
        save_routing_state(store.clone(), &base.id, &state)
            .await
            .context("failed to persist routing decision state")?;
        decision
    };

    let mut progress =
        reconcile_progress(github, candidate, &decision, config, state.progress.clone())
            .await
            .context("failed to reconcile existing side effects")?;

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
            state.mark_error(format!("decision comment write failed: {error:#}"), true);
            save_routing_state(store.clone(), &base.id, &state)
                .await
                .context("failed to persist ambiguous decision-comment error")?;
            progress = reconcile_progress(github, candidate, &decision, config, progress.clone())
                .await
                .context("failed to reconcile after decision comment error")?;
            if !progress.decision_comment_written {
                anyhow::bail!("decision comment write failed and could not be reconciled");
            }
        }
        progress.decision_comment_written = true;
        state.mark_progress(progress.clone());
        save_routing_state(store.clone(), &base.id, &state)
            .await
            .context("failed to persist decision comment progress")?;
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
            state.mark_error(format!("responsibility write failed: {error:#}"), true);
            save_routing_state(store.clone(), &base.id, &state)
                .await
                .context("failed to persist ambiguous responsibility error")?;
            progress = reconcile_progress(github, candidate, &decision, config, progress.clone())
                .await
                .context("failed to reconcile after responsibility write error")?;
            if !progress.responsibility_written {
                anyhow::bail!("responsibility write failed and could not be reconciled");
            }
        }
        progress.responsibility_written = true;
        state.mark_progress(progress.clone());
        save_routing_state(store.clone(), &base.id, &state)
            .await
            .context("failed to persist responsibility progress")?;
    }

    if !progress.project_status_updated {
        if let Err(error) = github
            .update_project_status(
                &candidate.project_id,
                &candidate.project_item_id,
                &decision.to_status,
            )
            .await
        {
            state.mark_error(format!("project status update failed: {error:#}"), true);
            save_routing_state(store.clone(), &base.id, &state)
                .await
                .context("failed to persist ambiguous project-status error")?;
            progress = reconcile_progress(github, candidate, &decision, config, progress.clone())
                .await
                .context("failed to reconcile after project status error")?;
            if !progress.project_status_updated {
                anyhow::bail!("project status update failed and could not be reconciled");
            }
        }
        progress.project_status_updated = true;
        state.mark_progress(progress.clone());
        save_routing_state(store.clone(), &base.id, &state)
            .await
            .context("failed to persist project-status progress")?;
    }

    state.clear_error();
    state.mark_progress(SideEffectProgress {
        decision_comment_written: true,
        responsibility_written: true,
        project_status_updated: true,
    });
    save_routing_state(store.clone(), &base.id, &state)
        .await
        .context("failed to persist completed routing state")?;

    reservation.decision_id = Some(decision.decision_id.clone());
    reservation.completed = true;
    save_reservation(store, &base.id, &reservation)
        .await
        .context("failed to persist completed reservation")?;

    Ok(())
}

async fn reserve_or_resume(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    config: &WorkgraphRouterReactionConfig,
    candidate: &RoutingCandidate,
    reservation_lock: Arc<Mutex<()>>,
) -> anyhow::Result<(ReservationRecord, RoutingStateRecord)> {
    // Smallest reusable reservation primitive available today:
    // the StateStoreProvider trait has no CAS/create-if-absent operation yet,
    // so we serialize read+write within the reaction process. Durable state
    // still guarantees replay-safe dedupe on restart for a single runner.
    let _guard = reservation_lock.lock().await;
    let reservation_key = candidate.reservation_key();
    let existing = load_reservation(store.clone(), store_id, &reservation_key).await?;
    let reservation = if let Some(existing) = existing {
        existing
    } else {
        let created = ReservationRecord {
            reservation_key: reservation_key.clone(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        save_reservation(store.clone(), store_id, &created).await?;
        created
    };

    let state = if let Some(existing_state) =
        load_routing_state(store.clone(), store_id, &reservation_key).await?
    {
        existing_state
    } else {
        let state = RoutingStateRecord::new(candidate, &reservation);
        save_routing_state(store, store_id, &state).await?;
        state
    };
    Ok((reservation, state))
}

async fn run_github_preflight(
    github: &GithubClient,
    candidate: &RoutingCandidate,
) -> anyhow::Result<()> {
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
        .current_project_status(&candidate.project_item_id)
        .await
        .context("GitHub project-status preflight failed")?;
    if current_status != "AwaitingRouting" {
        anyhow::bail!(
            "project item {} status is '{}' (expected 'AwaitingRouting')",
            candidate.project_item_id,
            current_status
        );
    }
    Ok(())
}
