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

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::CheckpointState;
use drasi_lib::reactions::ManagerCheckpointOwnership;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::{StateStoreCompareAndSwapResult, StateStoreProvider};
use drasi_lib::Reaction;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::candidate::RoutingCandidate;
use crate::config::{WorkgraphRouterReactionConfig, ROUTE_QUERY_ID};
use crate::decision::RoutingDecision;
use crate::github_client::{GithubClient, UpdateStatusOutcome};
use crate::reconciliation::reconcile_progress;
use crate::rules::{PolicyMode, RoutingPolicyEngine, RulesV1PolicyEngine};
use crate::state::{
    compare_and_swap_routing_state, create_reservation_if_absent, load_reservation_with_bytes,
    load_routing_state_with_bytes, reservation_store_key, routing_state_store_key,
    serialize_reservation, PersistedReservationRecord, ReservationRecord, RoutingStateRecord,
    SideEffectProgress,
};
use crate::validation::validate_candidate;
use crate::WorkgraphRouterReactionBuilder;

#[cfg(test)]
use crate::state::save_routing_state;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ReservationFencedError {
    message: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct PermanentCandidateError {
    reason_code: &'static str,
    message: String,
    owned_reservation: Option<OwnedReservation>,
}

impl PermanentCandidateError {
    fn new(reason_code: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason_code,
            message: message.into(),
            owned_reservation: None,
        }
    }

    fn with_reservation(mut self, reservation: &OwnedReservation) -> Self {
        self.owned_reservation = Some(reservation.clone());
        self
    }
}

#[derive(Debug, Clone)]
struct OwnedReservation {
    record: ReservationRecord,
    persisted_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct OwnedRoutingState {
    persisted_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueryResultProcessingOutcome {
    has_unresolved_nonterminal: bool,
}

const TERMINAL_REJECTION_SCHEMA: &str = "workgraph.router-rejection/v1";
const TERMINAL_REJECTION_PREFIX: &str = "workgraph-router/rejections/";
const TERMINAL_REJECTION_NAMESPACE: Uuid = Uuid::from_bytes([
    0x59, 0xc5, 0xd2, 0x5b, 0x7f, 0x94, 0x4d, 0x6d, 0x9b, 0x48, 0x42, 0x91, 0xf0, 0x6b, 0xa7, 0x35,
]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalRejectionRecord {
    schema_version: String,
    query_id: String,
    sequence: u64,
    row_signature: u64,
    row_fingerprint: String,
    reason_code: String,
    message: String,
    policy_id: String,
    policy_type: String,
    policy_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reservation_key: Option<String>,
    finalized: bool,
    rejected_at: String,
}

#[derive(Debug, Clone)]
struct SequenceCheckpointBarrier {
    unresolved_sequences: BTreeSet<u64>,
    completed_sequences: BTreeSet<u64>,
}

impl SequenceCheckpointBarrier {
    fn new() -> Self {
        Self {
            unresolved_sequences: BTreeSet::new(),
            completed_sequences: BTreeSet::new(),
        }
    }

    fn mark_completed(&mut self, sequence: u64) {
        self.unresolved_sequences.remove(&sequence);
        self.completed_sequences.insert(sequence);
    }

    fn mark_unresolved(&mut self, sequence: u64) {
        self.unresolved_sequences.insert(sequence);
        self.completed_sequences.remove(&sequence);
    }

    fn has_unresolved_before(&self, sequence: u64) -> bool {
        self.unresolved_sequences
            .iter()
            .next()
            .is_some_and(|blocked| *blocked < sequence)
    }

    async fn advance_ready(
        &mut self,
        base: &ReactionBase,
        checkpoint_state: &mut CheckpointState,
        query_id: &str,
    ) -> anyhow::Result<()> {
        while let Some(next_sequence) = self.completed_sequences.iter().next().copied() {
            if self
                .unresolved_sequences
                .iter()
                .next()
                .is_some_and(|blocked| *blocked < next_sequence)
            {
                break;
            }
            self.completed_sequences.remove(&next_sequence);
            checkpoint_state
                .advance(base, query_id, next_sequence)
                .await
                .with_context(|| {
                    format!(
                        "failed to advance checkpoint for query '{query_id}' to {next_sequence}"
                    )
                })?;
        }
        Ok(())
    }
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

    fn checkpoint_ownership(&self) -> ManagerCheckpointOwnership {
        ManagerCheckpointOwnership::Reaction
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
    let mut checkpoint_barrier = SequenceCheckpointBarrier::new();

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
        if checkpoint_barrier.has_unresolved_before(event.sequence) {
            info!(
                "[{}] deferring query '{}' sequence {} because earlier unresolved sequence exists",
                reaction_name, event.query_id, event.sequence
            );
            let requeued = base.priority_queue.enqueue(event.clone()).await;
            if !requeued {
                let message = format!(
                    "failed to requeue deferred query result {}:{}",
                    event.query_id, event.sequence
                );
                error!("[{reaction_name}] {message}");
                base.set_status(ComponentStatus::Error, Some(message)).await;
                return;
            }
            tokio::time::sleep(unresolved_retry_delay(&config)).await;
            continue;
        }

        let processing = match process_query_result(
            reaction_name,
            &base,
            &config,
            &github,
            runner_instance_id,
            &event,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
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
                checkpoint_barrier.mark_unresolved(event.sequence);
                let requeued = base.priority_queue.enqueue(event.clone()).await;
                if !requeued {
                    let message = format!(
                        "failed to requeue failed query result {}:{}",
                        event.query_id, event.sequence
                    );
                    error!("[{reaction_name}] {message}");
                    base.set_status(ComponentStatus::Error, Some(message)).await;
                    return;
                }
                tokio::time::sleep(unresolved_retry_delay(&config)).await;
                continue;
            }
        };

        if processing.has_unresolved_nonterminal {
            checkpoint_barrier.mark_unresolved(event.sequence);
            info!(
                "[{}] query '{}' sequence {} has unresolved fenced candidates; requeuing before checkpoint advance",
                reaction_name, event.query_id, event.sequence
            );
            let requeued = base.priority_queue.enqueue(event.clone()).await;
            if !requeued {
                let message = format!(
                    "failed to requeue unresolved query result {}:{}",
                    event.query_id, event.sequence
                );
                error!("[{reaction_name}] {message}");
                base.set_status(ComponentStatus::Error, Some(message)).await;
                return;
            }
            tokio::time::sleep(unresolved_retry_delay(&config)).await;
            continue;
        }

        checkpoint_barrier.mark_completed(event.sequence);
        if let Err(error) = checkpoint_barrier
            .advance_ready(&base, checkpoint_state, &event.query_id)
            .await
        {
            error!(
                "[{}] checkpoint update failed while evaluating sequence {}: {error:#}",
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
) -> anyhow::Result<QueryResultProcessingOutcome> {
    let mut has_unresolved_nonterminal = false;
    for diff in &result.results {
        match diff {
            ResultDiff::Add {
                data,
                row_signature,
            } => {
                if let Some(rejection) =
                    load_terminal_rejection(base, result, *row_signature, data).await?
                {
                    if rejection.finalized {
                        info!(
                            "[{reaction_name}] finalized terminal rejection already recorded for query '{}' sequence {} row {}; skipping replay",
                            result.query_id, result.sequence, row_signature
                        );
                        continue;
                    }

                    let candidate: RoutingCandidate = serde_json::from_value(data.clone())
                        .context(
                            "pending terminal rejection no longer contains a valid candidate",
                        )?;
                    if let Err(error) = complete_pending_terminal_rejection(
                        base,
                        config,
                        runner_instance_id,
                        &candidate,
                        &rejection,
                    )
                    .await
                    {
                        if error.downcast_ref::<ReservationFencedError>().is_some() {
                            has_unresolved_nonterminal = true;
                            info!(
                                "[{reaction_name}] terminal rejection tombstone for query '{}' sequence {} row {} remains fenced (will retry): {}",
                                result.query_id, result.sequence, row_signature, error
                            );
                            continue;
                        }
                        return Err(error);
                    }
                    finalize_terminal_rejection(base, result, *row_signature, data).await?;
                    continue;
                }
                let candidate: RoutingCandidate = match serde_json::from_value(data.clone()) {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        persist_terminal_rejection(
                            reaction_name,
                            base,
                            config,
                            result,
                            *row_signature,
                            data,
                            "invalid-row-shape",
                            &format!(
                                "failed to deserialize added row into RoutingCandidate: {error}"
                            ),
                            None,
                        )
                        .await?;
                        continue;
                    }
                };
                if let Err(error) = process_candidate(
                    reaction_name,
                    base,
                    config,
                    github,
                    runner_instance_id,
                    &candidate,
                )
                .await
                {
                    if let Some(permanent) = error.downcast_ref::<PermanentCandidateError>() {
                        let reservation_key = permanent
                            .owned_reservation
                            .as_ref()
                            .map(|owned| owned.record.reservation_key.as_str());
                        let rejection = persist_terminal_rejection(
                            reaction_name,
                            base,
                            config,
                            result,
                            *row_signature,
                            data,
                            permanent.reason_code,
                            &permanent.message,
                            reservation_key,
                        )
                        .await?;
                        if !rejection.finalized {
                            let mut owned_reservation = permanent
                                .owned_reservation
                                .clone()
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "pending terminal rejection is missing reservation ownership"
                                    )
                                })?;
                            let store = base.state_store().await.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "durable state store is required for workgraph-router"
                                )
                            })?;
                            complete_reservation(
                                store,
                                &base.id,
                                config,
                                runner_instance_id,
                                &mut owned_reservation,
                                &format!("terminal-rejection:{}", rejection.row_fingerprint),
                            )
                            .await
                            .context("failed to complete terminally rejected reservation")?;
                            finalize_terminal_rejection(base, result, *row_signature, data).await?;
                        }
                        continue;
                    }
                    if error.downcast_ref::<ReservationFencedError>().is_some() {
                        has_unresolved_nonterminal = true;
                        info!(
                            "[{}] reservation fenced for query '{}' sequence {} candidate '{}' (will retry): {}",
                            reaction_name,
                            result.query_id,
                            result.sequence,
                            candidate.reservation_key(),
                            error
                        );
                        continue;
                    }
                    return Err(error);
                }
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
    Ok(QueryResultProcessingOutcome {
        has_unresolved_nonterminal,
    })
}

fn terminal_rejection_store_key(
    query_id: &str,
    sequence: u64,
    row_signature: u64,
    row_fingerprint: &str,
) -> String {
    format!("{TERMINAL_REJECTION_PREFIX}{query_id}/{sequence}/{row_signature}/{row_fingerprint}")
}

fn terminal_rejection_fingerprint(data: &Value) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(data).context("failed to serialize rejected row fingerprint")?;
    Ok(Uuid::new_v5(&TERMINAL_REJECTION_NAMESPACE, &bytes).to_string())
}

async fn load_terminal_rejection(
    base: &ReactionBase,
    result: &QueryResult,
    row_signature: u64,
    data: &Value,
) -> anyhow::Result<Option<TerminalRejectionRecord>> {
    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("durable state store is required for workgraph-router"))?;
    let row_fingerprint = terminal_rejection_fingerprint(data)?;
    let key = terminal_rejection_store_key(
        &result.query_id,
        result.sequence,
        row_signature,
        &row_fingerprint,
    );
    let Some(existing) = store
        .get(&base.id, &key)
        .await
        .map_err(|error| anyhow::anyhow!("failed to read terminal rejection: {error}"))?
    else {
        return Ok(None);
    };
    let existing: TerminalRejectionRecord = serde_json::from_slice(&existing)
        .context("failed to deserialize existing terminal rejection record")?;
    validate_terminal_rejection_identity(&existing, result, row_signature, &row_fingerprint, &key)?;
    Ok(Some(existing))
}

fn validate_terminal_rejection_identity(
    record: &TerminalRejectionRecord,
    result: &QueryResult,
    row_signature: u64,
    row_fingerprint: &str,
    key: &str,
) -> anyhow::Result<()> {
    if record.schema_version != TERMINAL_REJECTION_SCHEMA
        || record.query_id != result.query_id
        || record.sequence != result.sequence
        || record.row_signature != row_signature
        || record.row_fingerprint != row_fingerprint
    {
        anyhow::bail!("terminal rejection key collision or conflicting replay for '{key}'");
    }
    Ok(())
}

async fn complete_pending_terminal_rejection(
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    runner_instance_id: &str,
    candidate: &RoutingCandidate,
    rejection: &TerminalRejectionRecord,
) -> anyhow::Result<()> {
    let expected_reservation_key = rejection.reservation_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("pending terminal rejection is missing its reservation key")
    })?;
    if candidate.reservation_key() != expected_reservation_key {
        anyhow::bail!(
            "pending terminal rejection reservation '{}' does not match candidate '{}'",
            expected_reservation_key,
            candidate.reservation_key()
        );
    }

    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("durable state store is required for workgraph-router"))?;
    let (reservation, mut owned_reservation, _, _) = reserve_or_resume(
        store.clone(),
        &base.id,
        config,
        candidate,
        runner_instance_id,
    )
    .await?;
    if reservation.reservation_key != expected_reservation_key {
        anyhow::bail!(
            "loaded reservation '{}' does not match pending terminal rejection '{}'",
            reservation.reservation_key,
            expected_reservation_key
        );
    }
    if reservation.completed {
        return Ok(());
    }

    let mut owned_reservation = owned_reservation
        .take()
        .ok_or_else(|| ReservationFencedError {
            message: format!(
                "terminal rejection reservation '{}' is fenced by owner '{}' epoch {}",
                reservation.reservation_key,
                reservation
                    .owner_instance_id
                    .as_deref()
                    .unwrap_or("unknown"),
                reservation.fencing_epoch
            ),
        })?;
    complete_reservation(
        store,
        &base.id,
        config,
        runner_instance_id,
        &mut owned_reservation,
        &format!("terminal-rejection:{}", rejection.row_fingerprint),
    )
    .await
}

async fn finalize_terminal_rejection(
    base: &ReactionBase,
    result: &QueryResult,
    row_signature: u64,
    data: &Value,
) -> anyhow::Result<()> {
    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("durable state store is required for workgraph-router"))?;
    let row_fingerprint = terminal_rejection_fingerprint(data)?;
    let key = terminal_rejection_store_key(
        &result.query_id,
        result.sequence,
        row_signature,
        &row_fingerprint,
    );

    loop {
        let existing_bytes = store
            .get(&base.id, &key)
            .await
            .map_err(|error| anyhow::anyhow!("failed to load terminal rejection: {error}"))?
            .ok_or_else(|| anyhow::anyhow!("terminal rejection '{key}' is missing"))?;
        let mut record: TerminalRejectionRecord = serde_json::from_slice(&existing_bytes)
            .context("failed to deserialize terminal rejection before finalization")?;
        validate_terminal_rejection_identity(
            &record,
            result,
            row_signature,
            &row_fingerprint,
            &key,
        )?;
        if record.finalized {
            return Ok(());
        }

        let reservation_key = record.reservation_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("pending terminal rejection is missing its reservation key")
        })?;
        let reservation = load_reservation_with_bytes(store.clone(), &base.id, reservation_key)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("terminal rejection reservation '{reservation_key}' is missing")
            })?
            .record;
        let expected_decision_id = format!("terminal-rejection:{row_fingerprint}");
        if !reservation.completed
            || reservation.decision_id.as_deref() != Some(expected_decision_id.as_str())
        {
            anyhow::bail!(
                "terminal rejection reservation '{reservation_key}' is not durably tombstoned"
            );
        }

        record.finalized = true;
        let finalized_bytes = serde_json::to_vec(&record)
            .context("failed to serialize finalized terminal rejection")?;
        match store
            .compare_and_swap(
                &base.id,
                &key,
                Some(existing_bytes.as_slice()),
                finalized_bytes,
            )
            .await
            .map_err(|error| anyhow::anyhow!("failed to finalize terminal rejection: {error}"))?
        {
            StateStoreCompareAndSwapResult::Swapped => return Ok(()),
            StateStoreCompareAndSwapResult::Mismatch => continue,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_terminal_rejection(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    result: &QueryResult,
    row_signature: u64,
    data: &Value,
    reason_code: &str,
    message: &str,
    reservation_key: Option<&str>,
) -> anyhow::Result<TerminalRejectionRecord> {
    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("durable state store is required for workgraph-router"))?;
    let row_fingerprint = terminal_rejection_fingerprint(data)?;
    let key = terminal_rejection_store_key(
        &result.query_id,
        result.sequence,
        row_signature,
        &row_fingerprint,
    );
    let record = TerminalRejectionRecord {
        schema_version: TERMINAL_REJECTION_SCHEMA.to_string(),
        query_id: result.query_id.clone(),
        sequence: result.sequence,
        row_signature,
        row_fingerprint: row_fingerprint.clone(),
        reason_code: reason_code.to_string(),
        message: message.chars().take(2_000).collect(),
        policy_id: config.policy_id.clone(),
        policy_type: config.policy_type.clone(),
        policy_version: config.policy_version.clone(),
        reservation_key: reservation_key.map(ToString::to_string),
        finalized: reservation_key.is_none(),
        rejected_at: chrono::Utc::now().to_rfc3339(),
    };
    let bytes =
        serde_json::to_vec(&record).context("failed to serialize terminal rejection record")?;

    match store
        .compare_and_swap(&base.id, &key, None, bytes)
        .await
        .map_err(|error| anyhow::anyhow!("failed to persist terminal rejection: {error}"))?
    {
        StateStoreCompareAndSwapResult::Swapped => {
            warn!(
                "[{reaction_name}] terminally rejected query '{}' sequence {} row {} ({reason_code}): {}",
                result.query_id,
                result.sequence,
                row_signature,
                record.message
            );
            Ok(record)
        }
        StateStoreCompareAndSwapResult::Mismatch => {
            let existing = store
                .get(&base.id, &key)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "failed to reload terminal rejection after CAS mismatch: {error}"
                    )
                })?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "terminal rejection CAS mismatched but record '{key}' disappeared"
                    )
                })?;
            let existing: TerminalRejectionRecord = serde_json::from_slice(&existing)
                .context("failed to deserialize existing terminal rejection record")?;
            validate_terminal_rejection_identity(
                &existing,
                result,
                row_signature,
                &row_fingerprint,
                &key,
            )?;
            if existing.reservation_key.as_deref() != reservation_key {
                anyhow::bail!(
                    "terminal rejection reservation identity changed for query '{}' sequence {} row {}",
                    result.query_id,
                    result.sequence,
                    row_signature
                );
            }
            Ok(existing)
        }
    }
}

async fn process_candidate(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    github: &GithubClient,
    runner_instance_id: &str,
    candidate: &RoutingCandidate,
) -> anyhow::Result<()> {
    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("durable state store is required for workgraph-router"))?;
    let (reservation, mut owned_reservation, mut state, mut owned_state) = reserve_or_resume(
        store.clone(),
        &base.id,
        config,
        candidate,
        runner_instance_id,
    )
    .await?;

    if reservation.completed {
        info!(
            "[{}] reservation '{}' already completed; skipping",
            reaction_name, reservation.reservation_key
        );
        return Ok(());
    }

    let mut owned_reservation = if let Some(owned) = owned_reservation.take() {
        owned
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

    if let Err(error) = validate_candidate(candidate, config) {
        let has_routing_history = state.decision.is_some()
            || state.selected_transition.is_some()
            || state.progress != SideEffectProgress::default()
            || state.ambiguous
            || state.failed;
        if has_routing_history {
            return Err(error).context(
                "row validation failed for an execution with existing routing history; refusing terminal rejection",
            );
        }
        return Err(PermanentCandidateError::new(
            "candidate-validation-failed",
            format!("row validation failed: {error:#}"),
        )
        .with_reservation(&owned_reservation)
        .into());
    }

    let reservation_policy_mismatch = reservation.policy_id != config.policy_id
        || reservation.policy_type != config.policy_type
        || reservation.policy_version != config.policy_version;

    let (decision, decision_is_new) = if let Some(decision) = state.decision.clone() {
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
        (decision, false)
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
        let outcome = engine.evaluate(candidate).map_err(|error| {
            PermanentCandidateError::new(
                "policy-evaluation-failed",
                format!("rules evaluation rejected candidate: {error:#}"),
            )
            .with_reservation(&owned_reservation)
        })?;
        let decision =
            RoutingDecision::from_policy(config, candidate, outcome).map_err(|error| {
                PermanentCandidateError::new(
                    "policy-output-rejected",
                    format!("policy output failed allowlist validation: {error:#}"),
                )
                .with_reservation(&owned_reservation)
            })?;
        state.selected_transition =
            Some((decision.from_status.clone(), decision.to_status.clone()));
        state.decision = Some(decision.clone());
        (decision, true)
    };

    if !config.allows_transition(&decision.from_status, &decision.to_status) {
        let message = format!(
            "selected transition {} -> {} is not allowlisted",
            decision.from_status, decision.to_status
        );
        if decision_is_new {
            return Err(
                PermanentCandidateError::new("transition-not-allowlisted", message)
                    .with_reservation(&owned_reservation)
                    .into(),
            );
        }
        anyhow::bail!(message);
    }

    if let Err(error) = decision.validate_allowlists(config) {
        if decision_is_new {
            return Err(PermanentCandidateError::new(
                "policy-output-rejected",
                format!("decision allowlist validation failed before side effects: {error:#}"),
            )
            .with_reservation(&owned_reservation)
            .into());
        }
        return Err(error).context("decision allowlist validation failed before side effects");
    }

    if decision_is_new {
        persist_state_with_ownership(
            store.clone(),
            &base.id,
            config,
            runner_instance_id,
            &mut owned_reservation,
            &mut owned_state,
            &state,
            "failed to persist routing decision state",
        )
        .await?;
    }

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
                    &mut owned_state,
                    &state,
                    "failed to persist preflight failure state",
                )
                .await?;
                return Err(error);
            }
        }

        if !progress.decision_comment_written {
            renew_reservation_ownership(
                store.clone(),
                &base.id,
                config,
                runner_instance_id,
                &mut owned_reservation,
                "decision comment write",
            )
            .await?;
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
                    &mut owned_state,
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
            &mut owned_state,
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
                    &mut owned_state,
                    &state,
                    "failed to persist preflight failure state",
                )
                .await?;
                return Err(error);
            }
        }

        if !progress.responsibility_written {
            renew_reservation_ownership(
                store.clone(),
                &base.id,
                config,
                runner_instance_id,
                &mut owned_reservation,
                "responsibility comment write",
            )
            .await?;
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
                    &mut owned_state,
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
            &mut owned_state,
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
                renew_reservation_ownership(
                    store.clone(),
                    &base.id,
                    config,
                    runner_instance_id,
                    &mut owned_reservation,
                    "project status write",
                )
                .await?;
                match github
                    .update_project_status(
                        &candidate.project_id,
                        &candidate.project_item_id,
                        &decision.from_status,
                        &decision.to_status,
                        &candidate.subject_repo,
                        candidate.subject_issue_number,
                    )
                    .await
                {
                    Ok(
                        UpdateStatusOutcome::Applied | UpdateStatusOutcome::AlreadyAtDestination,
                    ) => {}
                    Err(error) => {
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
                            &mut owned_state,
                            &state,
                            "failed to persist ambiguous project-status error",
                        )
                        .await?;
                        progress = reconcile_progress(
                            github,
                            candidate,
                            &decision,
                            config,
                            progress.clone(),
                        )
                        .await
                        .context("failed to reconcile after project status error")?;
                        if !progress.project_status_updated {
                            anyhow::bail!(
                                "project status update failed and could not be reconciled"
                            );
                        }
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
                    &mut owned_state,
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
            &mut owned_state,
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
        &mut owned_state,
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
    OwnedRoutingState,
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

        let (mut state, mut state_bytes) = if let Some(existing_state) =
            load_routing_state_with_bytes(store.clone(), store_id, &reservation_key).await?
        {
            (existing_state.record, Some(existing_state.bytes))
        } else {
            (RoutingStateRecord::new(candidate, &persisted.record), None)
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
            if state_bytes.is_none() {
                state.owner_instance_id = owned.record.owner_instance_id.clone();
                state.fencing_epoch = owned.record.fencing_epoch;
                let initialized = compare_and_swap_routing_state(
                    store.clone(),
                    store_id,
                    &reservation_key,
                    None,
                    &state,
                )
                .await
                .context("failed to initialize routing state for owned reservation")?;
                if initialized {
                    state_bytes = Some(
                        serde_json::to_vec(&state)
                            .context("failed to serialize initialized routing state")?,
                    );
                } else if let Some(existing_state) =
                    load_routing_state_with_bytes(store.clone(), store_id, &reservation_key).await?
                {
                    state = existing_state.record;
                    state_bytes = Some(existing_state.bytes);
                } else {
                    anyhow::bail!(
                        "routing state initialize CAS mismatched but state '{reservation_key}' is missing"
                    );
                }
            }
            return Ok((
                owned.record.clone(),
                Some(owned),
                state,
                OwnedRoutingState {
                    persisted_bytes: state_bytes,
                },
            ));
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
                if state_bytes.is_none() {
                    state.owner_instance_id = takeover.owner_instance_id.clone();
                    state.fencing_epoch = takeover.fencing_epoch;
                    let initialized = compare_and_swap_routing_state(
                        store.clone(),
                        store_id,
                        &reservation_key,
                        None,
                        &state,
                    )
                    .await
                    .context("failed to initialize routing state during takeover")?;
                    if initialized {
                        state_bytes = Some(
                            serde_json::to_vec(&state)
                                .context("failed to serialize initialized routing state")?,
                        );
                    } else if let Some(existing_state) =
                        load_routing_state_with_bytes(store.clone(), store_id, &reservation_key)
                            .await?
                    {
                        state = existing_state.record;
                        state_bytes = Some(existing_state.bytes);
                    } else {
                        anyhow::bail!(
                            "routing state initialize CAS mismatched but state '{reservation_key}' is missing"
                        );
                    }
                }
                let owned = OwnedReservation {
                    record: takeover.clone(),
                    persisted_bytes: new_bytes,
                };
                return Ok((
                    takeover,
                    Some(owned),
                    state,
                    OwnedRoutingState {
                        persisted_bytes: state_bytes,
                    },
                ));
            }
            continue;
        }

        return Ok((
            persisted.record,
            None,
            state,
            OwnedRoutingState {
                persisted_bytes: state_bytes,
            },
        ));
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
        .context("GitHub issue state preflight failed")?
    {
        anyhow::bail!(
            "subject issue {}/{} is not open",
            candidate.subject_repo,
            candidate.subject_issue_number
        );
    }
    github
        .validate_issue_snapshot(
            &candidate.subject_node_id,
            &candidate.subject_repo,
            candidate.subject_issue_number,
            &candidate.content_version,
        )
        .await
        .context("GitHub issue preflight failed")?;
    let current_status = github
        .current_project_status(
            &candidate.project_id,
            &candidate.project_item_id,
            &candidate.subject_repo,
            candidate.subject_issue_number,
        )
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

fn unresolved_retry_delay(config: &WorkgraphRouterReactionConfig) -> std::time::Duration {
    let lease_millis = config.reservation_lease_secs.saturating_mul(250);
    let bounded = lease_millis.clamp(100, 2_000);
    std::time::Duration::from_millis(bounded)
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

#[allow(clippy::too_many_arguments)]
async fn persist_state_with_ownership(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    config: &WorkgraphRouterReactionConfig,
    runner_instance_id: &str,
    owned: &mut OwnedReservation,
    routing_state: &mut OwnedRoutingState,
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

    let mut next_state = state.clone();
    next_state.owner_instance_id = owned.record.owner_instance_id.clone();
    next_state.fencing_epoch = owned.record.fencing_epoch;
    let next_state_bytes =
        serde_json::to_vec(&next_state).context("failed to serialize routing state")?;

    let swapped = compare_and_swap_routing_state(
        store.clone(),
        store_id,
        &next_state.reservation_key,
        routing_state.persisted_bytes.as_deref(),
        &next_state,
    )
    .await
    .with_context(|| error_context.to_string())?;
    if swapped {
        routing_state.persisted_bytes = Some(next_state_bytes);
        return Ok(());
    }

    let current = load_routing_state_with_bytes(store, store_id, &next_state.reservation_key)
        .await
        .with_context(|| error_context.to_string())
        .context("failed to reload routing state after CAS mismatch")?;
    if let Some(current) = current {
        if current.record.fencing_epoch > owned.record.fencing_epoch
            || current.record.owner_instance_id.as_deref() != Some(runner_instance_id)
        {
            return Err(ReservationFencedError {
                message: format!(
                    "routing state '{}' fenced before state update (owner='{}' epoch={})",
                    routing_state_store_key(&next_state.reservation_key),
                    current
                        .record
                        .owner_instance_id
                        .as_deref()
                        .unwrap_or("unknown"),
                    current.record.fencing_epoch
                ),
            }
            .into());
        }
        anyhow::bail!(
            "{}: routing-state CAS mismatch for owner '{}' epoch {}",
            error_context,
            current
                .record
                .owner_instance_id
                .as_deref()
                .unwrap_or("unknown"),
            current.record.fencing_epoch
        );
    }
    anyhow::bail!(
        "{}: routing-state CAS mismatch and record '{}' is missing",
        error_context,
        routing_state_store_key(&next_state.reservation_key)
    )
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
            trusted_routing_user_ids: vec![1001],
            trusted_launcher_user_ids: vec![1001],
            trusted_agent_user_ids: vec![1001],
            trusted_router_user_ids: vec![1001],
            trusted_router_author_node_ids: vec!["MDQ6VXNlcjE=".to_string()],
            expected_project_status_field_node_id: "PVTSSF_status".to_string(),
            timeout_secs: 5,
            reservation_lease_secs: 15,
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
            reason_code: "required-marker-present".to_string(),
            event_node_id: "workgraph-event:IC_event".to_string(),
            subject_repo: "drasi-project/drasi-core".to_string(),
            subject_issue_number: 42,
            subject_node_id: "I_issue".to_string(),
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
            launcher_author_id: 1001,
            agent_author: "agent-user".to_string(),
            agent_author_id: 1001,
            router_author: "router-user".to_string(),
            router_author_id: 1001,
            routing_author: "router-user".to_string(),
            routing_author_id: 1001,
            observed_authors: vec![
                "router-user".to_string(),
                "launcher-user".to_string(),
                "agent-user".to_string(),
            ],
            observed_author_ids: vec![1001, 1001, 1001],
            comment_id: 1,
            comment_author: "agent-user".to_string(),
            comment_body: "{\"ok\":true}".to_string(),
            comment_edited: false,
            comment_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            comment_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            comment_provenance_event_id: "event-1".to_string(),
            comment_provenance_event_type: "CompletedIssueValidation".to_string(),
            content_version: "sha256:abc".to_string(),
            content_profile: "phase2".to_string(),
            policy_id: "policy-1".to_string(),
            policy_type: "rules_v1".to_string(),
            policy_version: "1.0.0".to_string(),
        }
    }

    fn make_reaction(id: &str, config: &WorkgraphRouterReactionConfig) -> WorkgraphRouterReaction {
        WorkgraphRouterReaction::builder(id)
            .with_query(ROUTE_QUERY_ID)
            .with_config(config.clone())
            .build()
            .expect("reaction build")
    }

    async fn seed_completed_candidate_state(
        store: Arc<dyn StateStoreProvider>,
        store_id: &str,
        config: &WorkgraphRouterReactionConfig,
        candidate: &RoutingCandidate,
    ) {
        let reservation = ReservationRecord {
            reservation_key: candidate.reservation_key(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            owner_instance_id: Some("completed-owner".to_string()),
            fencing_epoch: 1,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: Some("done".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: true,
        };
        crate::state::save_reservation(store.clone(), store_id, &reservation)
            .await
            .expect("seed completed reservation");
        let mut state = RoutingStateRecord::new(candidate, &reservation);
        state.mark_progress(SideEffectProgress {
            decision_comment_written: true,
            responsibility_written: true,
            project_status_updated: true,
        });
        save_routing_state(store, store_id, &state)
            .await
            .expect("seed completed state");
    }

    async fn initialize_reaction_for_test(
        reaction: &WorkgraphRouterReaction,
        store: Arc<dyn StateStoreProvider>,
    ) {
        let (graph, _rx) =
            drasi_lib::component_graph::ComponentGraph::new("wg-router-reaction-test");
        let context = drasi_lib::context::ReactionRuntimeContext::new(
            "wg-router-reaction-test",
            reaction.id(),
            Some(store),
            graph.update_sender(),
            None,
        );
        reaction.initialize(context).await;
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

        let (_, owner_a, _, _) = a.expect("reserve a");
        let (_, owner_b, _, _) = b.expect("reserve b");
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
        let (_, owner_a, _, _) = a.expect("reserve a");
        let (_, owner_b, _, _) = b.expect("reserve b");
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

        let (_, owner_a, mut state_a, _) = reserve_or_resume(
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

        let (_, owner_b, _, _) = reserve_or_resume(
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
        let (reservation, owner, state, _) = reserve_or_resume(
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

    #[tokio::test]
    async fn multi_row_fenced_candidate_does_not_prevent_terminal_rejection() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let reaction = make_reaction("router-multi-row", &config);
        initialize_reaction_for_test(&reaction, store.clone()).await;

        let mut fenced_candidate = sample_candidate();
        fenced_candidate.execution_id = "exec-fenced".to_string();
        let fenced_reservation = ReservationRecord {
            reservation_key: fenced_candidate.reservation_key(),
            execution_id: fenced_candidate.execution_id.clone(),
            required_event_type: fenced_candidate.required_event_type.clone(),
            owner_instance_id: Some("other-owner".to_string()),
            fencing_epoch: 1,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        crate::state::save_reservation(store.clone(), "router-multi-row", &fenced_reservation)
            .await
            .expect("seed fenced reservation");

        let mut invalid_candidate = sample_candidate();
        invalid_candidate.execution_id = "exec-invalid".to_string();
        invalid_candidate.comment_edited = true;

        let result = QueryResult::new(
            ROUTE_QUERY_ID.to_string(),
            1,
            chrono::Utc::now(),
            vec![
                ResultDiff::Add {
                    data: serde_json::to_value(&fenced_candidate).expect("fenced candidate json"),
                    row_signature: 1,
                },
                ResultDiff::Add {
                    data: serde_json::to_value(&invalid_candidate).expect("invalid candidate json"),
                    row_signature: 2,
                },
            ],
            HashMap::new(),
        );

        let github = GithubClient::from_config(&config).expect("github client");
        let outcome = process_query_result(
            "router-multi-row",
            &reaction.base,
            &config,
            &github,
            &reaction.runner_instance_id,
            &result,
        )
        .await
        .expect("terminal rejection should not fail the query result");
        assert!(
            outcome.has_unresolved_nonterminal,
            "the fenced candidate must remain unresolved"
        );
        let rejected_data =
            serde_json::to_value(&invalid_candidate).expect("invalid candidate json");
        let fingerprint =
            terminal_rejection_fingerprint(&rejected_data).expect("rejection fingerprint");
        let rejection_key =
            terminal_rejection_store_key(ROUTE_QUERY_ID, result.sequence, 2, &fingerprint);
        let rejection = store
            .get("router-multi-row", &rejection_key)
            .await
            .expect("read terminal rejection")
            .expect("invalid candidate must be durably rejected");
        let rejection: TerminalRejectionRecord =
            serde_json::from_slice(&rejection).expect("valid rejection record");
        assert_eq!(rejection.reason_code, "candidate-validation-failed");
        assert!(rejection.finalized);
        assert_eq!(
            rejection.reservation_key.as_deref(),
            Some("exec-invalid:CompletedIssueValidation")
        );
        let reservation = crate::state::load_reservation(
            store,
            "router-multi-row",
            "exec-invalid:CompletedIssueValidation",
        )
        .await
        .expect("load invalid-candidate reservation")
        .expect("invalid candidate must retain a reservation tombstone");
        assert!(reservation.completed);
    }

    #[tokio::test]
    async fn validation_drift_cannot_terminalize_existing_routing_state() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let mut runtime_config = sample_config();
        runtime_config.trusted_agent_user_ids = vec![9999];
        let reaction = make_reaction("router-validation-drift", &runtime_config);
        initialize_reaction_for_test(&reaction, store.clone()).await;

        let candidate = sample_candidate();
        let reservation = ReservationRecord {
            reservation_key: candidate.reservation_key(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            owner_instance_id: Some(reaction.runner_instance_id.clone()),
            fencing_epoch: 1,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: runtime_config.policy_id.clone(),
            policy_type: runtime_config.policy_type.clone(),
            policy_version: runtime_config.policy_version.clone(),
            decision_id: Some("persisted-decision".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        crate::state::save_reservation(store.clone(), "router-validation-drift", &reservation)
            .await
            .expect("seed reservation");
        let mut state = RoutingStateRecord::new(&candidate, &reservation);
        state.progress.decision_comment_written = true;
        save_routing_state(store.clone(), "router-validation-drift", &state)
            .await
            .expect("seed partial routing state");

        let result = QueryResult::new(
            ROUTE_QUERY_ID.to_string(),
            1,
            chrono::Utc::now(),
            vec![ResultDiff::Add {
                data: serde_json::to_value(&candidate).expect("candidate json"),
                row_signature: 1,
            }],
            HashMap::new(),
        );
        let github = GithubClient::from_config(&runtime_config).expect("github client");
        let error = process_query_result(
            "router-validation-drift",
            &reaction.base,
            &runtime_config,
            &github,
            &reaction.runner_instance_id,
            &result,
        )
        .await
        .expect_err("validation drift over partial state must remain nonterminal");

        assert!(
            format!("{error:#}").contains("refusing terminal rejection"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            store
                .list_keys("router-validation-drift")
                .await
                .expect("list state")
                .iter()
                .filter(|key| key.starts_with(TERMINAL_REJECTION_PREFIX))
                .count(),
            0,
            "partial routing state must never gain a terminal rejection marker"
        );
    }

    #[tokio::test]
    async fn persisted_decision_must_still_pass_output_allowlists() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let mut runtime_config = sample_config();
        runtime_config.allowed_responsibility_types = vec!["issue-validation".to_string()];
        runtime_config.allowed_actors = vec!["bot-user".to_string(), "submitter-user".to_string()];
        let reaction = make_reaction("router-persisted-allowlist", &runtime_config);
        initialize_reaction_for_test(&reaction, store.clone()).await;

        let candidate = sample_candidate();
        let mut reservation = ReservationRecord {
            reservation_key: candidate.reservation_key(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            owner_instance_id: Some(reaction.runner_instance_id.clone()),
            fencing_epoch: 1,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: runtime_config.policy_id.clone(),
            policy_type: runtime_config.policy_type.clone(),
            policy_version: runtime_config.policy_version.clone(),
            decision_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        let decision = RoutingDecision::from_policy(
            &sample_config(),
            &candidate,
            RulesV1PolicyEngine
                .evaluate(&candidate)
                .expect("rules evaluation"),
        )
        .expect("decision");
        reservation.decision_id = Some(decision.decision_id.clone());
        crate::state::save_reservation(store.clone(), "router-persisted-allowlist", &reservation)
            .await
            .expect("seed reservation");

        let mut state = RoutingStateRecord::new(&candidate, &reservation);
        state.decision = Some(decision);
        state.selected_transition = Some((
            "AwaitingRouting".to_string(),
            "AwaitingIssueRiskProfiling".to_string(),
        ));
        save_routing_state(store, "router-persisted-allowlist", &state)
            .await
            .expect("seed state");

        let github = GithubClient::from_config(&runtime_config).expect("github client");
        let err = process_candidate(
            "router-persisted-allowlist",
            &reaction.base,
            &runtime_config,
            &github,
            &reaction.runner_instance_id,
            &candidate,
        )
        .await
        .expect_err("persisted decision should be blocked by output allowlist validation");
        assert!(
            err.to_string()
                .contains("decision allowlist validation failed before side effects"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn persisted_old_policy_decision_with_disallowed_transition_fails_without_checkpoint() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let mut runtime_config = sample_config();
        runtime_config.policy_version = "2.0.0".to_string();
        runtime_config.allowed_status_transitions = vec![crate::config::StatusTransition {
            from: "AwaitingRouting".to_string(),
            to: "NeedsMoreInformation".to_string(),
        }];

        let reaction = make_reaction("router-persisted-transition", &runtime_config);
        initialize_reaction_for_test(&reaction, store.clone()).await;

        let candidate = sample_candidate();
        let mut reservation = ReservationRecord {
            reservation_key: candidate.reservation_key(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            owner_instance_id: Some(reaction.runner_instance_id.clone()),
            fencing_epoch: 1,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: runtime_config.policy_id.clone(),
            policy_type: runtime_config.policy_type.clone(),
            policy_version: "1.0.0".to_string(),
            decision_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };

        let persisted_decision = RoutingDecision::from_policy(
            &sample_config(),
            &candidate,
            RulesV1PolicyEngine
                .evaluate(&candidate)
                .expect("rules evaluation"),
        )
        .expect("persisted decision");
        reservation.decision_id = Some(persisted_decision.decision_id.clone());
        crate::state::save_reservation(store.clone(), "router-persisted-transition", &reservation)
            .await
            .expect("seed reservation");

        let mut state = RoutingStateRecord::new(&candidate, &reservation);
        state.decision = Some(persisted_decision);
        state.selected_transition = Some((
            "AwaitingRouting".to_string(),
            "AwaitingIssueRiskProfiling".to_string(),
        ));
        save_routing_state(store, "router-persisted-transition", &state)
            .await
            .expect("seed routing state");

        reaction.start().await.expect("reaction start");
        reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&candidate).expect("candidate row"),
                    row_signature: 1,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue candidate");

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert_eq!(
            reaction.status().await,
            ComponentStatus::Error,
            "reaction should fail-safe when persisted transition is no longer allowlisted"
        );

        let checkpoint = reaction
            .base
            .read_checkpoint(ROUTE_QUERY_ID)
            .await
            .expect("read checkpoint")
            .map(|cp| cp.sequence)
            .unwrap_or(0);
        assert_eq!(
            checkpoint, 0,
            "checkpoint must not advance when persisted decision transition is rejected"
        );

        reaction.stop().await.expect("reaction stop");
    }

    #[tokio::test]
    async fn unresolved_result_does_not_advance_checkpoint_past_later_sequence() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let reaction = make_reaction("router-checkpoint-gap", &config);
        initialize_reaction_for_test(&reaction, store.clone()).await;

        let mut fenced_candidate = sample_candidate();
        fenced_candidate.execution_id = "exec-gap-fenced".to_string();
        let fenced_reservation = ReservationRecord {
            reservation_key: fenced_candidate.reservation_key(),
            execution_id: fenced_candidate.execution_id.clone(),
            required_event_type: fenced_candidate.required_event_type.clone(),
            owner_instance_id: Some("active-owner".to_string()),
            fencing_epoch: 1,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        crate::state::save_reservation(store.clone(), "router-checkpoint-gap", &fenced_reservation)
            .await
            .expect("seed fenced reservation");

        let mut completed_candidate = sample_candidate();
        completed_candidate.execution_id = "exec-gap-complete".to_string();
        let completed_reservation = ReservationRecord {
            reservation_key: completed_candidate.reservation_key(),
            execution_id: completed_candidate.execution_id.clone(),
            required_event_type: completed_candidate.required_event_type.clone(),
            owner_instance_id: Some("completed-owner".to_string()),
            fencing_epoch: 2,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: Some("done".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: true,
        };
        crate::state::save_reservation(
            store.clone(),
            "router-checkpoint-gap",
            &completed_reservation,
        )
        .await
        .expect("seed completed reservation");
        let mut completed_state =
            RoutingStateRecord::new(&completed_candidate, &completed_reservation);
        completed_state.mark_progress(SideEffectProgress {
            decision_comment_written: true,
            responsibility_written: true,
            project_status_updated: true,
        });
        save_routing_state(store.clone(), "router-checkpoint-gap", &completed_state)
            .await
            .expect("seed completed state");

        reaction.start().await.expect("reaction start");
        reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&fenced_candidate).expect("fenced row json"),
                    row_signature: 1,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue unresolved row");
        reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                2,
                chrono::Utc::now() + chrono::Duration::milliseconds(1),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&completed_candidate).expect("completed row json"),
                    row_signature: 2,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue later row");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        reaction.stop().await.expect("reaction stop");

        let checkpoint = reaction
            .base
            .read_checkpoint(ROUTE_QUERY_ID)
            .await
            .expect("read checkpoint");
        let sequence = checkpoint.map(|cp| cp.sequence).unwrap_or(0);
        assert!(
            sequence < 2,
            "checkpoint advanced past unresolved sequence: got {sequence}"
        );
    }

    #[tokio::test]
    async fn terminally_rejected_sequence_does_not_block_later_checkpoint() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let mut config = sample_config();
        config.strict_recovery = false;
        let reaction = make_reaction("router-nonstrict-failure-gap", &config);
        initialize_reaction_for_test(&reaction, store.clone()).await;

        let mut failed_candidate = sample_candidate();
        failed_candidate.execution_id = "exec-failure-gap".to_string();
        failed_candidate.comment_edited = true;

        let mut completed_candidate = sample_candidate();
        completed_candidate.execution_id = "exec-nonstrict-complete".to_string();
        seed_completed_candidate_state(
            store.clone(),
            "router-nonstrict-failure-gap",
            &config,
            &completed_candidate,
        )
        .await;

        reaction.start().await.expect("reaction start");
        reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&failed_candidate).expect("failed row json"),
                    row_signature: 1,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue failed row");
        reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                2,
                chrono::Utc::now() + chrono::Duration::milliseconds(1),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&completed_candidate).expect("completed row json"),
                    row_signature: 2,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue completed row");
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        reaction.stop().await.expect("reaction stop");

        let checkpoint = reaction
            .base
            .read_checkpoint(ROUTE_QUERY_ID)
            .await
            .expect("read checkpoint");
        let sequence = checkpoint.map(|cp| cp.sequence).unwrap_or(0);
        assert_eq!(
            sequence, 2,
            "terminal rejection must not poison checkpoint advancement"
        );
    }

    #[tokio::test]
    async fn nonstrict_unresolved_sequence_is_recoverable_before_checkpoint_advances() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let mut config = sample_config();
        config.strict_recovery = false;
        let reaction = make_reaction("router-nonstrict-recoverable", &config);
        initialize_reaction_for_test(&reaction, store.clone()).await;

        let mut fenced_candidate = sample_candidate();
        fenced_candidate.execution_id = "exec-nonstrict-fenced".to_string();
        let fenced_reservation = ReservationRecord {
            reservation_key: fenced_candidate.reservation_key(),
            execution_id: fenced_candidate.execution_id.clone(),
            required_event_type: fenced_candidate.required_event_type.clone(),
            owner_instance_id: Some("other-owner".to_string()),
            fencing_epoch: 1,
            lease_expires_at_unix_secs: chrono::Utc::now().timestamp() + 300,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            decision_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            completed: false,
        };
        crate::state::save_reservation(
            store.clone(),
            "router-nonstrict-recoverable",
            &fenced_reservation,
        )
        .await
        .expect("seed fenced reservation");

        let mut completed_candidate = sample_candidate();
        completed_candidate.execution_id = "exec-nonstrict-complete-2".to_string();
        seed_completed_candidate_state(
            store.clone(),
            "router-nonstrict-recoverable",
            &config,
            &completed_candidate,
        )
        .await;

        reaction.start().await.expect("reaction start");
        reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&fenced_candidate).expect("fenced row json"),
                    row_signature: 1,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue fenced row");
        reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                2,
                chrono::Utc::now() + chrono::Duration::milliseconds(1),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&completed_candidate).expect("completed row json"),
                    row_signature: 2,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue completed row");
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        reaction.stop().await.expect("reaction stop");

        let first_checkpoint = reaction
            .base
            .read_checkpoint(ROUTE_QUERY_ID)
            .await
            .expect("read first checkpoint")
            .map(|cp| cp.sequence)
            .unwrap_or(0);
        assert!(
            first_checkpoint < 2,
            "checkpoint advanced past unresolved sequence before recovery: got {first_checkpoint}"
        );

        seed_completed_candidate_state(
            store.clone(),
            "router-nonstrict-recoverable",
            &config,
            &fenced_candidate,
        )
        .await;

        let resumed_reaction = make_reaction("router-nonstrict-recoverable", &config);
        initialize_reaction_for_test(&resumed_reaction, store).await;
        resumed_reaction
            .start()
            .await
            .expect("resumed reaction start");
        resumed_reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&fenced_candidate).expect("fenced row json"),
                    row_signature: 1,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue recovered fenced row");
        resumed_reaction
            .enqueue_query_result(QueryResult::new(
                ROUTE_QUERY_ID.to_string(),
                2,
                chrono::Utc::now() + chrono::Duration::milliseconds(1),
                vec![ResultDiff::Add {
                    data: serde_json::to_value(&completed_candidate).expect("completed row json"),
                    row_signature: 2,
                }],
                HashMap::new(),
            ))
            .await
            .expect("enqueue completed row");
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        resumed_reaction
            .stop()
            .await
            .expect("resumed reaction stop");

        let recovered_checkpoint = resumed_reaction
            .base
            .read_checkpoint(ROUTE_QUERY_ID)
            .await
            .expect("read recovered checkpoint")
            .map(|cp| cp.sequence)
            .unwrap_or(0);
        assert!(
            recovered_checkpoint >= 2,
            "checkpoint did not advance after unresolved sequence became recoverable: got {recovered_checkpoint}"
        );
    }

    #[tokio::test]
    async fn stale_owner_routing_state_cas_rejected_after_takeover() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let candidate = sample_candidate();
        let reaction_a = make_reaction("router-state-a", &config);
        let reaction_b = make_reaction("router-state-b", &config);

        let (_, owner_a, mut state_a, mut owned_state_a) = reserve_or_resume(
            store.clone(),
            "router-state-store",
            &config,
            &candidate,
            &reaction_a.runner_instance_id,
        )
        .await
        .expect("owner a reserve");
        let mut owner_a = owner_a.expect("owner a");

        state_a.mark_progress(SideEffectProgress {
            decision_comment_written: true,
            responsibility_written: false,
            project_status_updated: false,
        });
        persist_state_with_ownership(
            store.clone(),
            "router-state-store",
            &config,
            &reaction_a.runner_instance_id,
            &mut owner_a,
            &mut owned_state_a,
            &state_a,
            "persist owner a state",
        )
        .await
        .expect("persist owner a");
        let stale_bytes = owned_state_a
            .persisted_bytes
            .clone()
            .expect("owner a persisted state bytes");

        owner_a.record.lease_expires_at_unix_secs = chrono::Utc::now().timestamp() - 1;
        crate::state::save_reservation(store.clone(), "router-state-store", &owner_a.record)
            .await
            .expect("expire owner a lease");
        owner_a.persisted_bytes =
            serialize_reservation(&owner_a.record).expect("serialize owner a");

        let (_, owner_b, mut state_b, mut owned_state_b) = reserve_or_resume(
            store.clone(),
            "router-state-store",
            &config,
            &candidate,
            &reaction_b.runner_instance_id,
        )
        .await
        .expect("owner b takeover");
        let mut owner_b = owner_b.expect("owner b");
        state_b.mark_progress(SideEffectProgress {
            decision_comment_written: true,
            responsibility_written: true,
            project_status_updated: true,
        });
        persist_state_with_ownership(
            store.clone(),
            "router-state-store",
            &config,
            &reaction_b.runner_instance_id,
            &mut owner_b,
            &mut owned_state_b,
            &state_b,
            "persist owner b state",
        )
        .await
        .expect("persist owner b");

        let swapped = compare_and_swap_routing_state(
            store.clone(),
            "router-state-store",
            &candidate.reservation_key(),
            Some(stale_bytes.as_slice()),
            &state_a,
        )
        .await
        .expect("stale routing-state CAS");
        assert!(!swapped, "stale owner bytes must not overwrite newer state");

        let stale_err = persist_state_with_ownership(
            store,
            "router-state-store",
            &config,
            &reaction_a.runner_instance_id,
            &mut owner_a,
            &mut owned_state_a,
            &state_a,
            "stale owner persist attempt",
        )
        .await
        .expect_err("stale owner must be fenced");
        assert!(stale_err.downcast_ref::<ReservationFencedError>().is_some());
    }

    #[tokio::test]
    async fn interleaved_state_writes_preserve_newer_progress() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
        let config = sample_config();
        let candidate = sample_candidate();
        let reaction_a = make_reaction("router-interleave-a", &config);
        let reaction_b = make_reaction("router-interleave-b", &config);

        let (_, owner_a, mut state_a, mut owned_state_a) = reserve_or_resume(
            store.clone(),
            "router-interleave-store",
            &config,
            &candidate,
            &reaction_a.runner_instance_id,
        )
        .await
        .expect("owner a reserve");
        let mut owner_a = owner_a.expect("owner a");
        state_a.mark_progress(SideEffectProgress {
            decision_comment_written: true,
            responsibility_written: false,
            project_status_updated: false,
        });
        persist_state_with_ownership(
            store.clone(),
            "router-interleave-store",
            &config,
            &reaction_a.runner_instance_id,
            &mut owner_a,
            &mut owned_state_a,
            &state_a,
            "persist owner a interleave state",
        )
        .await
        .expect("persist owner a");
        owner_a.record.lease_expires_at_unix_secs = chrono::Utc::now().timestamp() - 1;
        crate::state::save_reservation(store.clone(), "router-interleave-store", &owner_a.record)
            .await
            .expect("expire owner a");
        owner_a.persisted_bytes =
            serialize_reservation(&owner_a.record).expect("serialize owner a");

        let (_, owner_b, mut state_b, mut owned_state_b) = reserve_or_resume(
            store.clone(),
            "router-interleave-store",
            &config,
            &candidate,
            &reaction_b.runner_instance_id,
        )
        .await
        .expect("owner b takeover");
        let mut owner_b = owner_b.expect("owner b");
        state_b.mark_progress(SideEffectProgress {
            decision_comment_written: true,
            responsibility_written: true,
            project_status_updated: true,
        });
        persist_state_with_ownership(
            store.clone(),
            "router-interleave-store",
            &config,
            &reaction_b.runner_instance_id,
            &mut owner_b,
            &mut owned_state_b,
            &state_b,
            "persist owner b interleave state",
        )
        .await
        .expect("persist owner b");

        let current = load_routing_state_with_bytes(
            store.clone(),
            "router-interleave-store",
            &candidate.reservation_key(),
        )
        .await
        .expect("load current state")
        .expect("state present");
        assert_eq!(
            current.record.owner_instance_id.as_deref(),
            owner_b.record.owner_instance_id.as_deref()
        );
        assert_eq!(current.record.fencing_epoch, owner_b.record.fencing_epoch);
        assert!(current.record.progress.project_status_updated);
    }
}
