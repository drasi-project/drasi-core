// Copyright 2025 The Drasi Authors.
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

use anyhow::{Context, Result};
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::channels::*;
use crate::component_graph::{ComponentGraph, ComponentKind, ComponentUpdateSender};
use crate::config::ReactionRuntime;
use crate::context::ReactionRuntimeContext;
use crate::identity::IdentityProvider;
use crate::managers::{log_component_error, ComponentLogKey, ComponentLogRegistry};
use crate::metrics::{
    LifecycleMetrics, ReactionMetrics, RecoveryPolicyKind, StartupRejectionReason,
};
use crate::queries::output_state::FetchError;
use crate::queries::Query;
use crate::reactions::bootstrap_context::BootstrapContext;
use crate::reactions::checkpoint::ReactionCheckpoint;
use crate::reactions::snapshot_fetcher::InProcessSnapshotFetcher;
use crate::reactions::{QueryProvider, Reaction};
use crate::recovery::ReactionRecoveryPolicy;
use crate::state_store::StateStoreProvider;

/// Key for per-reaction per-query metrics: `(reaction_id, query_id)`.
type MetricsKey = (String, String);

/// Convert a domain recovery policy to the metrics-level kind enum.
fn to_policy_kind(policy: &ReactionRecoveryPolicy) -> RecoveryPolicyKind {
    match policy {
        ReactionRecoveryPolicy::Strict => RecoveryPolicyKind::Strict,
        ReactionRecoveryPolicy::AutoReset => RecoveryPolicyKind::AutoReset,
        ReactionRecoveryPolicy::AutoSkipGap => RecoveryPolicyKind::AutoSkipGap,
    }
}

/// Context passed to `handle_broadcast_gap` to avoid excessive parameter counts.
///
/// Groups the shared forwarder-task state that the recovery function needs.
struct BroadcastGapContext<'a> {
    reaction_id: &'a str,
    query_id: &'a str,
    reaction: &'a Arc<dyn Reaction>,
    query: &'a Arc<dyn Query>,
    policy: ReactionRecoveryPolicy,
    state_store: &'a Option<Arc<dyn StateStoreProvider>>,
    checkpoints: &'a Arc<RwLock<HashMap<String, ReactionCheckpoint>>>,
    bootstrap_mutex: &'a Arc<tokio::sync::Mutex<()>>,
    metrics: &'a Arc<ReactionMetrics>,
    /// The live event that exposed a sequence gap. `None` when the receiver
    /// reported lag before yielding the next retained event.
    received_sequence: Option<u64>,
}

struct ReactionSubscriptionTasks {
    forwarder_abort_handles: Vec<tokio::task::AbortHandle>,
    supervisor: tokio::task::JoinHandle<()>,
}

pub struct ReactionManager {
    instance_id: String,
    /// Query provider for reactions to access queries (injected after DrasiLib is constructed)
    query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
    /// State store provider for reactions to persist state
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Identity provider for credential injection
    identity_provider: Arc<RwLock<Option<Arc<dyn IdentityProvider>>>>,
    /// Log registry for component log streaming
    log_registry: Arc<ComponentLogRegistry>,
    /// Subscription forwarders and their reaping supervisor per reaction.
    subscription_tasks: Arc<RwLock<HashMap<String, ReactionSubscriptionTasks>>>,
    /// Shared component graph — the single source of truth for component metadata,
    /// state, relationships, runtime instances, AND event history.
    graph: Arc<RwLock<ComponentGraph>>,
    /// Channel sender for routing status updates through the graph update loop.
    /// Managers send transitional states (Starting, Stopping, Reconfiguring) here;
    /// the loop applies them to the graph and records events automatically.
    update_tx: ComponentUpdateSender,
    /// Per-(reaction, query) metrics for observability.
    reaction_metrics: Arc<RwLock<HashMap<MetricsKey, Arc<ReactionMetrics>>>>,
    /// Global lifecycle metrics shared across all reactions.
    lifecycle_metrics: Arc<LifecycleMetrics>,
}

impl ReactionManager {
    /// Create a new ReactionManager
    ///
    /// # Parameters
    /// - `instance_id`: The DrasiLib instance ID for log routing
    /// - `log_registry`: Shared log registry for component log streaming
    /// - `graph`: Shared component graph for tracking component relationships and emitting events
    pub fn new(
        instance_id: impl Into<String>,
        log_registry: Arc<ComponentLogRegistry>,
        graph: Arc<RwLock<ComponentGraph>>,
        update_tx: ComponentUpdateSender,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            query_provider: Arc::new(RwLock::new(None)),
            state_store: Arc::new(RwLock::new(None)),
            identity_provider: Arc::new(RwLock::new(None)),
            log_registry,
            subscription_tasks: Arc::new(RwLock::new(HashMap::new())),
            graph,
            update_tx,
            reaction_metrics: Arc::new(RwLock::new(HashMap::new())),
            lifecycle_metrics: Arc::new(LifecycleMetrics::new()),
        }
    }

    /// Inject the query provider (called after DrasiLib is fully constructed)
    ///
    /// This allows the ReactionManager to provide query access to reactions.
    pub async fn inject_query_provider(&self, qp: Arc<dyn QueryProvider>) {
        *self.query_provider.write().await = Some(qp);
    }

    /// Inject the state store provider (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to access the state store when they are added.
    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
    }

    /// Inject the identity provider (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to obtain authentication credentials when they are added.
    pub async fn inject_identity_provider(&self, identity_provider: Arc<dyn IdentityProvider>) {
        *self.identity_provider.write().await = Some(identity_provider);
    }

    /// Add a reaction instance, taking ownership and wrapping it in an Arc internally.
    ///
    /// This method handles runtime-only operations: creating the runtime context,
    /// initializing the reaction, and storing it in the runtime map. Graph registration
    /// (node creation, dependency edges) must be done by the caller beforehand via
    /// `ComponentGraph::register_reaction()`.
    ///
    /// # Parameters
    /// - `reaction`: The reaction instance to provision (ownership is transferred)
    ///
    /// # Note
    /// The reaction will NOT be auto-started. Call `start_reaction` separately
    /// if you need to start it after adding.
    pub async fn provision_reaction(&self, reaction: impl Reaction + 'static) -> Result<()> {
        let reaction: Arc<dyn Reaction> = Arc::new(reaction);
        let reaction_id = reaction.id().to_string();

        // Build snapshot fetcher scoped to this reaction's query IDs
        let snapshot_fetcher = Arc::new(InProcessSnapshotFetcher::new(
            self.query_provider.clone(),
            reaction.query_ids(),
        ));

        // Construct runtime context for this reaction
        let mut context = ReactionRuntimeContext::new(
            &self.instance_id,
            &reaction_id,
            self.state_store.read().await.clone(),
            self.update_tx.clone(),
            None,
        );
        context.identity_provider = self.identity_provider.read().await.clone();
        context.snapshot_fetcher = Some(snapshot_fetcher);

        // Initialize the reaction with its runtime context
        reaction.initialize(context).await;

        // Store the runtime instance in the graph
        {
            let mut graph = self.graph.write().await;
            graph.set_runtime(&reaction_id, Box::new(reaction))?;
        }

        info!("Provisioned reaction: {reaction_id}");

        Ok(())
    }

    /// Start a reaction.
    ///
    /// The reaction must have been added via `add_reaction()` first, which injects
    /// the necessary dependencies (event channel and query subscriber).
    ///
    /// Performs startup validation (§3), then:
    /// 1. Starts the reaction (spawns processing loop which waits on bootstrap gate)
    /// 2. Wires live subscriptions (events buffer in priority queue)
    /// 3. Runs per-query bootstrap (§5) — reads checkpoints, fetches snapshot/outbox,
    ///    applies recovery policy, invokes `bootstrap()` hook if needed
    /// 4. Opens the bootstrap gate — processing loop begins draining
    ///
    /// # Parameters
    /// - `id`: The reaction ID to start
    pub async fn start_reaction(&self, id: String) -> Result<()> {
        let reaction =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Reaction>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new(
                        "reaction", &id,
                    ))
                })?;

        // --- §3 Startup validation ---
        self.validate_startup_config(&reaction).await?;

        self.prepare_start_generation(&id, &reaction).await?;

        if let Err(error) = crate::managers::lifecycle_helpers::start_component(
            &self.graph,
            &id,
            "reaction",
            &reaction,
        )
        .await
        {
            self.cleanup_failed_start(&id, &reaction, format!("Start failed: {error}"))
                .await;
            return Err(error);
        }

        // Create the bootstrap gate — forwarders wait on this before processing.
        // Using a watch channel (not Notify) so late subscribers see the current value
        // and cannot miss the notification.
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);

        if let Err(error) = self
            .subscribe_and_bootstrap(&id, reaction.clone(), gate_rx)
            .await
        {
            self.cleanup_failed_start(&id, &reaction, format!("Bootstrap failed: {error}"))
                .await;
            return Err(error);
        }

        // Open the gate — forwarders begin draining buffered events.
        let _ = gate_tx.send(true);

        Ok(())
    }

    async fn prepare_start_generation(
        &self,
        reaction_id: &str,
        reaction: &Arc<dyn Reaction>,
    ) -> Result<()> {
        Self::abort_subscription_tasks_static(&self.subscription_tasks, reaction_id).await;

        let status = self
            .graph
            .read()
            .await
            .get_component(reaction_id)
            .map(|node| node.status)
            .ok_or_else(|| anyhow::anyhow!("Reaction '{reaction_id}' not found"))?;
        if status != ComponentStatus::Error {
            return Ok(());
        }

        reaction
            .stop()
            .await
            .with_context(|| format!("Failed to clean prior generation for '{reaction_id}'"))?;
        crate::component_graph::wait_for_status(
            &self.graph,
            reaction_id,
            &[ComponentStatus::Stopped],
            std::time::Duration::from_secs(2),
        )
        .await
        .with_context(|| {
            format!("Reaction '{reaction_id}' did not stop before retrying its start")
        })?;
        Ok(())
    }

    async fn cleanup_failed_start(
        &self,
        reaction_id: &str,
        reaction: &Arc<dyn Reaction>,
        message: String,
    ) {
        {
            let mut graph = self.graph.write().await;
            let _ = graph.validate_and_transition(
                reaction_id,
                ComponentStatus::Error,
                Some(message.clone()),
            );
        }

        Self::abort_subscription_tasks_static(&self.subscription_tasks, reaction_id).await;
        if let Err(cleanup_error) = reaction.stop().await {
            log::error!(
                "[{reaction_id}] Failed to clean reaction after start failure: {cleanup_error}"
            );
            return;
        }

        if let Err(wait_error) = crate::component_graph::wait_for_status(
            &self.graph,
            reaction_id,
            &[ComponentStatus::Stopped],
            std::time::Duration::from_secs(2),
        )
        .await
        {
            log::error!("[{reaction_id}] Failed-start cleanup did not reach Stopped: {wait_error}");
            return;
        }

        let mut graph = self.graph.write().await;
        let _ = graph.validate_and_transition(reaction_id, ComponentStatus::Error, Some(message));
    }

    /// Validate the reaction's startup configuration (§3 compatibility rules).
    ///
    /// 1. `is_durable=true` requires a durable state store
    /// 2. `needs_snapshot_on_fresh_start=true` + `AutoSkipGap` → reject (contradictory)
    /// 3. `needs_snapshot_on_fresh_start=false` + `AutoReset` → reject (AutoReset needs
    ///    snapshot capability to re-bootstrap)
    async fn validate_startup_config(&self, reaction: &Arc<dyn Reaction>) -> Result<()> {
        let is_durable = reaction.is_durable();
        let needs_snapshot = reaction.needs_snapshot_on_fresh_start();
        let policy = reaction.default_recovery_policy();

        // Rule 1: durable reaction requires durable state store
        if is_durable {
            let store = self.state_store.read().await;
            match store.as_ref() {
                None => {
                    self.lifecycle_metrics
                        .record_startup_rejection(StartupRejectionReason::DurableNoStore);
                    return Err(anyhow::anyhow!(
                        "Reaction '{}' requires a durable state store (is_durable=true), \
                         but no state store is configured",
                        reaction.id()
                    ));
                }
                Some(s) if !s.is_durable() => {
                    self.lifecycle_metrics
                        .record_startup_rejection(StartupRejectionReason::DurableOnVolatile);
                    return Err(anyhow::anyhow!(
                        "Reaction '{}' requires a durable state store (is_durable=true), \
                         but the configured state store is volatile",
                        reaction.id()
                    ));
                }
                _ => {}
            }
        }

        // Rule 2: snapshot + AutoSkipGap is contradictory
        if needs_snapshot && policy == ReactionRecoveryPolicy::AutoSkipGap {
            self.lifecycle_metrics
                .record_startup_rejection(StartupRejectionReason::SnapshotSkipGap);
            return Err(anyhow::anyhow!(
                "Reaction '{}': needs_snapshot_on_fresh_start=true is incompatible with \
                 AutoSkipGap recovery policy (cannot skip gap if snapshot is required)",
                reaction.id()
            ));
        }

        // Rule 3: !snapshot + AutoReset is contradictory
        if !needs_snapshot && policy == ReactionRecoveryPolicy::AutoReset {
            self.lifecycle_metrics
                .record_startup_rejection(StartupRejectionReason::NoSnapshotAutoReset);
            return Err(anyhow::anyhow!(
                "Reaction '{}': needs_snapshot_on_fresh_start=false is incompatible with \
                 AutoReset recovery policy (AutoReset re-bootstraps from a snapshot)",
                reaction.id()
            ));
        }

        Ok(())
    }

    /// Wire live subscriptions, then perform per-query bootstrap.
    ///
    /// 1. Wire subscriptions first (forwarders wait on gate, events buffer in broadcast channel)
    /// 2. Read checkpoints from state store
    /// 3. For each query, run the per-query startup flowchart (§5)
    /// 4. Invoke `reaction.bootstrap()` if any query needed a full reset
    ///
    /// The bootstrap gate is NOT opened here — the caller opens it after this returns.
    async fn subscribe_and_bootstrap(
        &self,
        reaction_id: &str,
        reaction: Arc<dyn Reaction>,
        gate: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let query_ids = reaction.query_ids();
        if query_ids.is_empty() {
            return Ok(());
        }

        // Clone the query provider Arc and release the RwLock guard immediately.
        let query_provider = self.query_provider.read().await.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "QueryProvider not injected - was ReactionManager initialized properly?"
            )
        })?;

        let state_store = self.state_store.read().await.clone();
        let policy = reaction.default_recovery_policy();

        // Shared checkpoint map — populated during bootstrap, read by forwarders after gate opens.
        let shared_checkpoints: Arc<RwLock<HashMap<String, ReactionCheckpoint>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // 1. Wire subscriptions FIRST so events buffer in broadcast channels
        //    while bootstrap runs. Forwarders wait on gate before processing.
        self.wire_subscriptions(
            reaction_id,
            &reaction,
            &query_provider,
            &query_ids,
            shared_checkpoints.clone(),
            gate,
        )
        .await?;

        // 2. Read existing checkpoints from the state store (batch).
        let existing_checkpoints = match state_store.as_ref() {
            Some(store) => {
                crate::reactions::checkpoint::read_checkpoints_batch(
                    store.as_ref(),
                    reaction_id,
                    &query_ids,
                )
                .await?
            }
            None => HashMap::new(),
        };

        // 3. Per-query bootstrap: build the initial checkpoint map and collect
        //    any queries that need a full bootstrap hook call.
        let mut initial_checkpoints: HashMap<String, ReactionCheckpoint> = HashMap::new();
        let mut bootstrap_queries: Vec<(String, Arc<dyn Query>)> = Vec::new();

        for query_id in &query_ids {
            let query = query_provider.get_query_instance(query_id).await?;

            // Pre-create the metrics entry for this (reaction, query) pair so all
            // bootstrap methods can instrument without acquiring the write lock.
            let metrics_key = (reaction_id.to_string(), query_id.clone());
            let per_query_metrics = {
                let mut metrics_map = self.reaction_metrics.write().await;
                metrics_map
                    .entry(metrics_key)
                    .or_insert_with(|| Arc::new(ReactionMetrics::new()))
                    .clone()
            };

            match existing_checkpoints.get(query_id) {
                None => {
                    // No checkpoint — fresh start for this query.
                    let cp = self
                        .handle_fresh_start(
                            reaction_id,
                            query_id,
                            &reaction,
                            &query,
                            &state_store,
                            &mut bootstrap_queries,
                            &per_query_metrics,
                        )
                        .await?;
                    initial_checkpoints.insert(query_id.clone(), cp);
                }
                Some(cp) => {
                    // Checkpoint exists — check config hash.
                    let current_config_hash =
                        crate::queries::compute_config_hash(query.get_config());
                    if cp.config_hash != current_config_hash {
                        // Hash mismatch → treat as gap → apply recovery policy.
                        self.lifecycle_metrics.record_hash_mismatch();
                        info!(
                            "[{reaction_id}] Config hash mismatch for query '{query_id}': \
                             checkpoint={}, current={}",
                            cp.config_hash, current_config_hash
                        );
                        let new_cp = self
                            .apply_recovery_policy(
                                reaction_id,
                                query_id,
                                &reaction,
                                &query,
                                policy,
                                &state_store,
                                &mut bootstrap_queries,
                                &per_query_metrics,
                            )
                            .await?;
                        initial_checkpoints.insert(query_id.clone(), new_cp);
                    } else {
                        // Hash matches — try to catch up via outbox.
                        let new_cp = self
                            .handle_outbox_catchup(
                                reaction_id,
                                query_id,
                                cp,
                                &reaction,
                                &query,
                                policy,
                                &state_store,
                                &mut bootstrap_queries,
                                &per_query_metrics,
                            )
                            .await?;
                        initial_checkpoints.insert(query_id.clone(), new_cp);
                    }
                }
            }
        }

        // 4. Invoke the bootstrap hook if any queries triggered a reset.
        if !bootstrap_queries.is_empty() {
            for (query_id, query) in &bootstrap_queries {
                let is_reset = existing_checkpoints.contains_key(query_id);
                let ctx = BootstrapContext::new(
                    query_id.clone(),
                    is_reset,
                    query.clone(),
                    reaction_id.to_string(),
                    state_store.clone(),
                );
                reaction.bootstrap(ctx).await?;
            }
        }

        // 5. Persist checkpoints AFTER bootstrap succeeds — a crash before this
        //    point will re-trigger bootstrap on next start (safe).
        if let Some(store) = state_store.as_ref() {
            for (query_id, cp) in &initial_checkpoints {
                if let Err(e) = crate::reactions::checkpoint::write_checkpoint(
                    store.as_ref(),
                    reaction_id,
                    query_id,
                    cp,
                )
                .await
                {
                    log::warn!(
                        "[{reaction_id}] Failed to persist checkpoint for query '{query_id}' \
                         at seq={}: {e}",
                        cp.sequence
                    );
                }
            }
        }

        // Publish the computed checkpoints so forwarders can filter stale events.
        *shared_checkpoints.write().await = initial_checkpoints;

        Ok(())
    }

    /// Handle a fresh start for a query subscription (no existing checkpoint).
    ///
    /// If `needs_snapshot_on_fresh_start`, fetches snapshot and sets checkpoint.
    /// Otherwise, fetches outbox(0) to get the current sequence.
    async fn handle_fresh_start(
        &self,
        reaction_id: &str,
        query_id: &str,
        reaction: &Arc<dyn Reaction>,
        query: &Arc<dyn Query>,
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        bootstrap_queries: &mut Vec<(String, Arc<dyn Query>)>,
        metrics: &Arc<ReactionMetrics>,
    ) -> Result<ReactionCheckpoint> {
        let config_hash = crate::queries::compute_config_hash(query.get_config());

        if reaction.needs_snapshot_on_fresh_start() {
            info!("[{reaction_id}] Fresh start for query '{query_id}' — fetching snapshot");
            metrics.record_fetch_snapshot();
            let snapshot = query.fetch_snapshot().await.map_err(|e| {
                anyhow::anyhow!("Failed to fetch snapshot for query '{query_id}': {e}")
            })?;

            let cp = ReactionCheckpoint {
                sequence: snapshot.as_of_sequence,
                config_hash,
            };

            bootstrap_queries.push((query_id.to_string(), query.clone()));
            Ok(cp)
        } else {
            // No snapshot needed — replay any outbox entries produced during this
            // startup cycle (e.g., from source replay) and record the checkpoint.
            // Without this replay, results produced before the reaction subscribes
            // to the broadcast channel would be silently skipped.
            metrics.record_fetch_outbox();
            let seq = match query.fetch_outbox(0).await {
                Ok(resp) => {
                    if resp.results.is_empty() {
                        info!(
                            "[{reaction_id}] Fresh start for query '{query_id}' — fetch_outbox(0) returned latest_seq={}",
                            resp.latest_sequence
                        );
                    } else {
                        info!(
                            "[{reaction_id}] Fresh start for query '{query_id}' — replaying {} outbox entries (latest_seq={})",
                            resp.results.len(),
                            resp.latest_sequence
                        );
                        for entry in &resp.results {
                            let result = (*entry).as_ref().clone();
                            reaction
                                .enqueue_query_result(result)
                                .await
                                .with_context(|| {
                                    format!(
                                        "[{reaction_id}] failed to replay outbox entry for query \
                                     '{query_id}' at sequence {}",
                                        entry.sequence
                                    )
                                })?;
                        }
                    }
                    resp.latest_sequence
                }
                Err(FetchError::OutboxGap(gap)) => {
                    info!(
                        "[{reaction_id}] Fresh start for query '{query_id}' — outbox gap, latest_seq={}",
                        gap.latest_sequence
                    );
                    gap.latest_sequence
                }
                Err(FetchError::NotRunning { .. } | FetchError::TimedOut) => {
                    info!(
                        "[{reaction_id}] Fresh start for query '{query_id}' — \
                         query not yet running, starting from sequence 0"
                    );
                    0
                }
            };

            let cp = ReactionCheckpoint {
                sequence: seq,
                config_hash,
            };

            if let Some(store) = state_store.as_ref() {
                crate::reactions::checkpoint::write_checkpoint(
                    store.as_ref(),
                    reaction_id,
                    query_id,
                    &cp,
                )
                .await?;
            }

            Ok(cp)
        }
    }

    /// Handle outbox catchup when a checkpoint exists and hash matches.
    ///
    /// Fetches outbox entries after the checkpoint sequence. If the outbox
    /// returns a gap, applies the recovery policy.
    #[allow(clippy::too_many_arguments)]
    async fn handle_outbox_catchup(
        &self,
        reaction_id: &str,
        query_id: &str,
        checkpoint: &ReactionCheckpoint,
        reaction: &Arc<dyn Reaction>,
        query: &Arc<dyn Query>,
        policy: ReactionRecoveryPolicy,
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        bootstrap_queries: &mut Vec<(String, Arc<dyn Query>)>,
        metrics: &Arc<ReactionMetrics>,
    ) -> Result<ReactionCheckpoint> {
        metrics.record_fetch_outbox();
        match query.fetch_outbox(checkpoint.sequence).await {
            Ok(outbox_resp) => {
                // Replay outbox entries by enqueuing them.
                // Track the last successfully enqueued sequence to avoid
                // advancing the checkpoint past failed entries.
                let mut last_ok_seq = checkpoint.sequence;
                for entry in &outbox_resp.results {
                    let result = (*entry).as_ref().clone();
                    reaction
                        .enqueue_query_result(result)
                        .await
                        .with_context(|| {
                            format!(
                                "[{reaction_id}] failed to replay outbox entry for query \
                             '{query_id}' at sequence {}",
                                entry.sequence
                            )
                        })?;
                    last_ok_seq = entry.sequence;
                }

                // Update checkpoint to the latest SUCCESSFULLY replayed sequence.
                let new_seq = last_ok_seq;

                let cp = ReactionCheckpoint {
                    sequence: new_seq,
                    config_hash: checkpoint.config_hash,
                };

                if new_seq != checkpoint.sequence {
                    if let Some(store) = state_store.as_ref() {
                        crate::reactions::checkpoint::write_checkpoint(
                            store.as_ref(),
                            reaction_id,
                            query_id,
                            &cp,
                        )
                        .await?;
                    }
                }

                Ok(cp)
            }
            Err(FetchError::OutboxGap(gap)) => {
                info!(
                    "[{reaction_id}] Outbox gap for query '{query_id}' — applying recovery policy"
                );
                match policy {
                    ReactionRecoveryPolicy::AutoSkipGap => {
                        Self::replay_retained_outbox_after_gap(
                            reaction_id,
                            query_id,
                            checkpoint,
                            reaction,
                            query,
                            state_store,
                            metrics,
                            &gap,
                        )
                        .await
                    }
                    ReactionRecoveryPolicy::Strict | ReactionRecoveryPolicy::AutoReset => {
                        self.apply_recovery_policy(
                            reaction_id,
                            query_id,
                            reaction,
                            query,
                            policy,
                            state_store,
                            bootstrap_queries,
                            metrics,
                        )
                        .await
                    }
                }
            }
            Err(FetchError::NotRunning { .. } | FetchError::TimedOut) => {
                // Query not running — keep the existing checkpoint as-is.
                // The forwarder will pick up events once the query starts.
                warn!(
                    "[{reaction_id}] Query '{query_id}' not running during catchup — \
                     keeping existing checkpoint at seq={}",
                    checkpoint.sequence
                );
                Ok(checkpoint.clone())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn replay_retained_outbox_after_gap(
        reaction_id: &str,
        query_id: &str,
        checkpoint: &ReactionCheckpoint,
        reaction: &Arc<dyn Reaction>,
        query: &Arc<dyn Query>,
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        metrics: &Arc<ReactionMetrics>,
        gap: &crate::queries::OutboxGap,
    ) -> Result<ReactionCheckpoint> {
        let mut skipped_floor = gap
            .earliest_available
            .saturating_sub(1)
            .max(checkpoint.sequence);
        let mut current = ReactionCheckpoint {
            sequence: skipped_floor,
            config_hash: checkpoint.config_hash,
        };
        let mut previous_sequence = checkpoint.sequence;

        let retained = loop {
            Self::persist_reaction_checkpoint(
                state_store,
                reaction_id,
                query_id,
                previous_sequence,
                &current,
            )
            .await?;
            previous_sequence = current.sequence;

            metrics.record_fetch_outbox();
            match query.fetch_outbox(skipped_floor).await {
                Ok(retained) => break retained,
                Err(FetchError::OutboxGap(next_gap)) => {
                    let next_floor = next_gap
                        .earliest_available
                        .saturating_sub(1)
                        .max(current.sequence);
                    anyhow::ensure!(
                        next_floor > skipped_floor,
                        "[{reaction_id}] query '{query_id}' returned a non-advancing \
                             outbox gap at floor {skipped_floor}"
                    );
                    skipped_floor = next_floor;
                    current.sequence = next_floor;
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "[{reaction_id}] failed to fetch retained outbox suffix for query \
                             '{query_id}' after skipped floor {skipped_floor}: {error}"
                    ));
                }
            }
        };

        let mut expected = skipped_floor.saturating_add(1);
        for entry in retained.results {
            anyhow::ensure!(
                entry.sequence == expected,
                "[{reaction_id}] retained outbox suffix for query '{query_id}' is not \
                     contiguous: expected {expected}, got {}",
                entry.sequence
            );
            reaction
                .enqueue_query_result(entry.as_ref().clone())
                .await
                .with_context(|| {
                    format!(
                        "[{reaction_id}] failed to replay retained outbox entry for query \
                             '{query_id}' at sequence {}",
                        entry.sequence
                    )
                })?;
            current.sequence = entry.sequence;
            Self::persist_reaction_checkpoint(
                state_store,
                reaction_id,
                query_id,
                previous_sequence,
                &current,
            )
            .await?;
            previous_sequence = current.sequence;
            expected = entry.sequence.saturating_add(1);
        }
        anyhow::ensure!(
            current.sequence == retained.latest_sequence,
            "[{reaction_id}] retained outbox replay for query '{query_id}' ended at {}, \
                 expected latest {}",
            current.sequence,
            retained.latest_sequence
        );

        Ok(current)
    }

    async fn persist_reaction_checkpoint(
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        reaction_id: &str,
        query_id: &str,
        previous_sequence: u64,
        checkpoint: &ReactionCheckpoint,
    ) -> Result<()> {
        if checkpoint.sequence == previous_sequence {
            return Ok(());
        }
        if let Some(store) = state_store.as_ref() {
            crate::reactions::checkpoint::write_checkpoint(
                store.as_ref(),
                reaction_id,
                query_id,
                checkpoint,
            )
            .await?;
        }
        Ok(())
    }

    /// Apply the recovery policy when a gap or hash mismatch is detected.
    #[allow(clippy::too_many_arguments)]
    async fn apply_recovery_policy(
        &self,
        reaction_id: &str,
        query_id: &str,
        _reaction: &Arc<dyn Reaction>,
        query: &Arc<dyn Query>,
        policy: ReactionRecoveryPolicy,
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        bootstrap_queries: &mut Vec<(String, Arc<dyn Query>)>,
        metrics: &Arc<ReactionMetrics>,
    ) -> Result<ReactionCheckpoint> {
        let config_hash = crate::queries::compute_config_hash(query.get_config());

        match policy {
            ReactionRecoveryPolicy::Strict => Err(anyhow::anyhow!(
                "Reaction '{reaction_id}': Strict recovery policy — cannot recover from \
                     gap/mismatch for query '{query_id}'. Manual intervention required."
            )),
            ReactionRecoveryPolicy::AutoReset => {
                info!(
                    "[{reaction_id}] AutoReset for query '{query_id}' — \
                     fetching fresh snapshot"
                );
                metrics.record_fetch_snapshot();
                let snapshot = query.fetch_snapshot().await.map_err(|e| {
                    anyhow::anyhow!(
                        "AutoReset: failed to fetch snapshot for query '{query_id}': {e}"
                    )
                })?;

                let cp = ReactionCheckpoint {
                    sequence: snapshot.as_of_sequence,
                    config_hash,
                };

                // Record successful auto-reset in lifecycle metrics
                self.lifecycle_metrics.record_auto_reset_completion();

                bootstrap_queries.push((query_id.to_string(), query.clone()));
                Ok(cp)
            }
            ReactionRecoveryPolicy::AutoSkipGap => {
                info!(
                    "[{reaction_id}] AutoSkipGap for query '{query_id}' — \
                     jumping to current sequence"
                );

                // Get current sequence from the query.
                metrics.record_fetch_outbox();
                let current_seq = match query.fetch_outbox(0).await {
                    Ok(resp) => resp.latest_sequence,
                    Err(FetchError::OutboxGap(gap)) => gap.latest_sequence,
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "AutoSkipGap: failed to determine current sequence \
                             for query '{query_id}': {e}"
                        ));
                    }
                };

                let cp = ReactionCheckpoint {
                    sequence: current_seq,
                    config_hash,
                };

                if let Some(store) = state_store.as_ref() {
                    crate::reactions::checkpoint::write_checkpoint(
                        store.as_ref(),
                        reaction_id,
                        query_id,
                        &cp,
                    )
                    .await?;
                }

                Ok(cp)
            }
        }
    }

    /// Stop a running reaction and abort its subscription forwarder tasks.
    ///
    /// # Errors
    /// Returns an error if the reaction is not found or the stop operation fails.
    pub async fn stop_reaction(&self, id: String) -> Result<()> {
        let reaction =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Reaction>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new(
                        "reaction", &id,
                    ))
                })?;

        // Transition to Stopping FIRST so the supervisor won't race us
        // and transition to Error when forwarder tasks are aborted.
        {
            let mut g = self.graph.write().await;
            g.validate_and_transition(
                &id,
                ComponentStatus::Stopping,
                Some("Stopping reaction".to_string()),
            )?;
        }

        // Now abort subscription forwarder tasks
        self.abort_subscription_tasks(&id).await;

        // Call the reaction's stop logic
        if let Err(e) = reaction.stop().await {
            let mut g = self.graph.write().await;
            let _ = g.validate_and_transition(
                &id,
                ComponentStatus::Error,
                Some(format!("Stop failed: {e}")),
            );
            return Err(e);
        }

        Ok(())
    }

    /// Returns the current status of a reaction (e.g. Running, Stopped, Error).
    ///
    /// # Errors
    /// Returns an error if the reaction is not found.
    pub async fn get_reaction_status(&self, id: String) -> Result<ComponentStatus> {
        crate::managers::lifecycle_helpers::get_component_status(&self.graph, &id, "Reaction").await
    }

    /// Retrieve a reaction's runtime descriptor, including its status, subscribed queries, and properties.
    ///
    /// # Errors
    /// Returns an error if the reaction is not found.
    pub async fn get_reaction(&self, id: String) -> Result<ReactionRuntime> {
        let graph = self.graph.read().await;
        let reaction = graph.get_runtime::<Arc<dyn Reaction>>(&id).cloned();

        if let Some(reaction) = reaction {
            let status = graph
                .get_component(&id)
                .map(|n| n.status)
                .unwrap_or(ComponentStatus::Stopped);
            let error_message = match &status {
                ComponentStatus::Error => graph.get_last_error(&id),
                _ => None,
            };
            drop(graph);
            let runtime = ReactionRuntime {
                id: reaction.id().to_string(),
                reaction_type: reaction.type_name().to_string(),
                status,
                error_message,
                queries: reaction.query_ids(),
                properties: reaction.properties(),
            };
            Ok(runtime)
        } else {
            Err(crate::managers::ComponentNotFoundError::new("reaction", &id).into())
        }
    }

    /// Teardown a reaction's runtime state — stop, deprovision, and remove from runtime map.
    ///
    /// This method handles runtime-only operations. Graph deregistration
    /// (node removal, edge cleanup) must be done by the caller afterwards via
    /// `ComponentGraph::deregister()`.
    ///
    /// The caller should validate dependencies via `graph.can_remove()` before calling this.
    pub async fn teardown_reaction(&self, id: String, cleanup: bool) -> Result<()> {
        let id_clone = id.clone();
        let sub_tasks = self.subscription_tasks.clone();
        crate::managers::lifecycle_helpers::teardown_component::<Arc<dyn Reaction>, _, _>(
            &self.graph,
            &id,
            "reaction",
            ComponentType::Reaction,
            &self.instance_id,
            &self.log_registry,
            cleanup,
            || Self::abort_subscription_tasks_static(&sub_tasks, &id_clone),
        )
        .await?;

        // Also abort any remaining subscription tasks after teardown
        self.abort_subscription_tasks(&id).await;

        // Remove metrics entries for this reaction to prevent unbounded memory growth
        {
            let mut metrics_map = self.reaction_metrics.write().await;
            metrics_map.retain(|(rid, _), _| rid != &id);
        }

        Ok(())
    }

    /// Update a reaction by replacing it with a new instance.
    ///
    /// Flow: validate exists → validate status → set Reconfiguring via graph →
    /// stop if running/starting → wait for stopped → initialize new →
    /// replace (if still exists) → restart if was running.
    /// Log and event history are preserved.
    pub async fn update_reaction(
        &self,
        id: String,
        new_reaction: impl Reaction + 'static,
    ) -> Result<()> {
        let old_reaction = {
            let graph = self.graph.read().await;
            graph.get_runtime::<Arc<dyn Reaction>>(&id).cloned()
        };

        if let Some(old_reaction) = old_reaction {
            // Verify the new reaction has the same ID
            if new_reaction.id() != id {
                return Err(anyhow::anyhow!(
                    "New reaction ID '{}' does not match existing reaction ID '{}'",
                    new_reaction.id(),
                    id
                ));
            }

            let graph = &self.graph;
            let instance_id = &self.instance_id;
            let state_store = &self.state_store;
            let update_tx = &self.update_tx;

            crate::managers::lifecycle_helpers::reconfigure_component::<Arc<dyn Reaction>, _, _, _>(
                graph,
                &id,
                "reaction",
                &old_reaction,
                || self.abort_subscription_tasks(&id),
                || async {
                    let new_reaction: Arc<dyn Reaction> = Arc::new(new_reaction);
                    let snapshot_fetcher = Arc::new(InProcessSnapshotFetcher::new(
                        self.query_provider.clone(),
                        new_reaction.query_ids(),
                    ));
                    let mut context = ReactionRuntimeContext::new(
                        instance_id,
                        &id,
                        state_store.read().await.clone(),
                        update_tx.clone(),
                        None,
                    );
                    context.snapshot_fetcher = Some(snapshot_fetcher);
                    new_reaction.initialize(context).await;

                    let mut g = graph.write().await;
                    if !g.has_runtime(&id) {
                        return Err(anyhow::anyhow!(
                            "Reaction '{id}' was concurrently deleted during reconfiguration"
                        ));
                    }
                    g.set_runtime(&id, Box::new(new_reaction))?;
                    Ok(())
                },
                || self.start_reaction(id.clone()),
            )
            .await
        } else {
            Err(crate::managers::ComponentNotFoundError::new("reaction", &id).into())
        }
    }

    /// List all registered reactions with their current statuses.
    pub async fn list_reactions(&self) -> Vec<(String, ComponentStatus)> {
        crate::managers::lifecycle_helpers::list_components(&self.graph, &ComponentKind::Reaction)
            .await
    }

    /// Start all reactions that have `auto_start` enabled.
    ///
    /// Reactions must have been added via `add_reaction()` first, which injects
    /// the necessary dependencies (event channel and query subscriber).
    ///
    /// Only reactions with `auto_start() == true` will be started.
    pub async fn start_all(&self) -> Result<()> {
        crate::managers::lifecycle_helpers::start_all_components::<Arc<dyn Reaction>, _, _>(
            &self.graph,
            &ComponentKind::Reaction,
            "reaction",
            |r| r.auto_start(),
            |id, _reaction| self.start_reaction(id),
        )
        .await
    }

    /// Stop all currently running reactions.
    ///
    /// # Errors
    /// Returns an error if any reaction fails to stop.
    pub async fn stop_all(&self) -> Result<()> {
        crate::managers::lifecycle_helpers::stop_all_components(
            &self.graph,
            &ComponentKind::Reaction,
            "Reaction",
            |id| self.stop_reaction(id),
        )
        .await
    }

    /// Record a component event in the history.
    ///
    /// This should be called by the event processing loop to track component
    /// lifecycle events for later querying.
    pub async fn record_event(&self, event: ComponentEvent) {
        let mut graph = self.graph.write().await;
        graph.record_event(event);
    }

    /// Get events for a specific reaction.
    ///
    /// Returns events in chronological order (oldest first).
    pub async fn get_reaction_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.graph.read().await.get_events(id)
    }

    /// Get all events across all reactions.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        let graph = self.graph.read().await;
        graph
            .get_all_events()
            .into_iter()
            .filter(|e| e.component_type == ComponentType::Reaction)
            .collect()
    }

    /// Subscribe to live logs for a reaction.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// Returns None if the reaction doesn't exist.
    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> Option<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        // Verify the reaction exists in the graph
        {
            let graph = self.graph.read().await;
            if !graph.has_runtime(id) {
                return None;
            }
        }

        let log_key = ComponentLogKey::new(&self.instance_id, ComponentType::Reaction, id);
        Some(self.log_registry.subscribe_by_key(&log_key).await)
    }

    /// Subscribe to live events for a reaction.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// Returns None if the reaction doesn't exist.
    pub async fn subscribe_events(
        &self,
        id: &str,
    ) -> Option<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let graph = self.graph.read().await;
        if !graph.has_runtime(id) {
            return None;
        }
        graph.subscribe_events(id)
    }

    /// Wire live broadcast subscriptions with gap detection (§6) and bootstrap gate.
    ///
    /// For each query:
    /// 1. Subscribe to the query's result broadcast channel
    /// 2. Spawn a forwarder task that waits on the bootstrap gate, then drains events
    /// 3. On `RecvError::Lagged`, apply the recovery policy
    async fn wire_subscriptions(
        &self,
        reaction_id: &str,
        reaction: &Arc<dyn Reaction>,
        query_provider: &Arc<dyn QueryProvider>,
        query_ids: &[String],
        shared_checkpoints: Arc<RwLock<HashMap<String, ReactionCheckpoint>>>,
        gate: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let instance_id = self.instance_id.clone();
        let policy = reaction.default_recovery_policy();
        let state_store = self.state_store.read().await.clone();
        let mut abort_handles: Vec<tokio::task::AbortHandle> = Vec::new();
        let mut join_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // Mutex to serialize concurrent bootstrap calls (§9 — multi-query gap recovery).
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        for query_id in query_ids {
            let query = match query_provider.get_query_instance(query_id).await {
                Ok(query) => query,
                Err(error) => {
                    Self::abort_and_reap_forwarders(abort_handles, join_handles).await;
                    return Err(error);
                }
            };

            let subscription = match query.subscribe(reaction_id.to_string()).await {
                Ok(subscription) => subscription,
                Err(error) => {
                    Self::abort_and_reap_forwarders(abort_handles, join_handles).await;
                    return Err(error);
                }
            };
            let mut receiver = subscription.receiver;

            // Create or retrieve per-(reaction, query) metrics
            let metrics_key = (reaction_id.to_string(), query_id.clone());
            let forwarder_metrics = {
                let mut metrics_map = self.reaction_metrics.write().await;
                metrics_map
                    .entry(metrics_key)
                    .or_insert_with(|| Arc::new(ReactionMetrics::new()))
                    .clone()
            };

            let reaction = reaction.clone();
            let query_id_clone = query_id.clone();
            let reaction_id_owned = reaction_id.to_string();
            let mut gate_rx = gate.clone();
            let query_clone = query.clone();
            let state_store_clone = state_store.clone();
            let checkpoints = shared_checkpoints.clone();
            let bootstrap_mutex = bootstrap_mutex.clone();
            let lifecycle_metrics = self.lifecycle_metrics.clone();

            let span = tracing::info_span!(
                "reaction_forwarder",
                instance_id = %instance_id,
                component_id = %reaction_id_owned,
                component_type = "reaction"
            );

            let forwarder_task = tokio::spawn(
                async move {
                    // Wait for the bootstrap gate to open before processing.
                    // watch::wait_for retains the value, so even late subscribers see it.
                    // If the sender is dropped (bootstrap failed), exit immediately.
                    if gate_rx.wait_for(|v| *v).await.is_err() {
                        log::debug!(
                            "[{reaction_id_owned}] Gate sender dropped for query '{query_id_clone}' \
                             — exiting forwarder (bootstrap likely failed)"
                        );
                        return;
                    }

                    // Read the initial checkpoint sequence so we can skip stale events
                    // that were buffered in the broadcast channel during bootstrap.
                    let initial_seq = {
                        let cps = checkpoints.read().await;
                        cps.get(&query_id_clone).map(|cp| cp.sequence).unwrap_or(0)
                    };

                    // Track the last forwarded sequence for gap detection.
                    let mut last_forwarded_seq = initial_seq;

                    log::debug!(
                        "[{reaction_id_owned}] Started result forwarder for query '{query_id_clone}' \
                         (initial_seq={initial_seq})"
                    );

                    loop {
                        match receiver.recv().await {
                            Ok(query_result) => {
                                if query_result.sequence == 0
                                    && query_result
                                        .metadata
                                        .get("control_signal")
                                        .and_then(serde_json::Value::as_str)
                                        .is_some_and(|signal| {
                                            matches!(
                                                signal,
                                                "bootstrapStarted" | "bootstrapCompleted"
                                            )
                                        })
                                {
                                    let result = Arc::try_unwrap(query_result)
                                        .unwrap_or_else(|arc| (*arc).clone());
                                    if let Err(error) =
                                        reaction.enqueue_query_result(result).await
                                    {
                                        log::error!(
                                            "[{reaction_id_owned}] Failed to enqueue bootstrap control \
                                             from query '{query_id_clone}': {error}"
                                        );
                                        break;
                                    }
                                    continue;
                                }

                                // Skip events already covered by the bootstrap snapshot/outbox catchup.
                                if query_result.sequence <= last_forwarded_seq {
                                    forwarder_metrics.record_dedup_skip();
                                    log::debug!(
                                        "[{reaction_id_owned}] Skipping seq={} <= last_forwarded={last_forwarded_seq} for query '{query_id_clone}'",
                                        query_result.sequence
                                    );
                                    continue;
                                }

                                // §6: Detect sequence gaps (incoming seq > expected next).
                                // This catches drops that don't manifest as broadcast lag
                                // (e.g., outbox overflow while reaction was stopping).
                                if query_result.sequence
                                    > last_forwarded_seq.saturating_add(1)
                                {
                                    forwarder_metrics.record_gap_detection();
                                    forwarder_metrics.record_recovery_trigger(to_policy_kind(&policy));
                                    log::warn!(
                                        "[{reaction_id_owned}] Sequence gap for query '{query_id_clone}': \
                                         expected seq={}, got seq={}",
                                        last_forwarded_seq.saturating_add(1),
                                        query_result.sequence
                                    );
                                    let gap_ctx = BroadcastGapContext {
                                        reaction_id: &reaction_id_owned,
                                        query_id: &query_id_clone,
                                        reaction: &reaction,
                                        query: &query_clone,
                                        policy,
                                        state_store: &state_store_clone,
                                        checkpoints: &checkpoints,
                                        bootstrap_mutex: &bootstrap_mutex,
                                        metrics: &forwarder_metrics,
                                        received_sequence: Some(query_result.sequence),
                                    };
                                    match Self::handle_broadcast_gap(&gap_ctx)
                                    .await
                                    {
                                        Ok(()) => {
                                            // After gap recovery, update checkpoint from the shared map
                                            // (handle_broadcast_gap updates it).
                                            let cps = checkpoints.read().await;
                                            last_forwarded_seq = cps
                                                .get(&query_id_clone)
                                                .map(|cp| cp.sequence)
                                                .unwrap_or(last_forwarded_seq);
                                            // Re-evaluate this event against the updated checkpoint.
                                            if query_result.sequence <= last_forwarded_seq {
                                                continue;
                                            }
                                        }
                                        Err(e) => {
                                            log::error!(
                                                "[{reaction_id_owned}] Recovery failed for sequence gap \
                                                 on query '{query_id_clone}': {e}"
                                            );
                                            break;
                                        }
                                    }
                                }

                                match Self::enqueue_forwarded_result(
                                    &reaction,
                                    &query_clone,
                                    &query_id_clone,
                                    &checkpoints,
                                    &forwarder_metrics,
                                    query_result,
                                )
                                .await
                                {
                                    Ok(sequence) => last_forwarded_seq = sequence,
                                    Err(error) => {
                                        log::error!(
                                            "[{reaction_id_owned}] Failed to enqueue result from query \
                                             '{query_id_clone}': {error}"
                                        );
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                if error_str.contains("lagged") {
                                    // §6: Broadcast gap detected.
                                    forwarder_metrics.record_gap_detection();
                                    forwarder_metrics.record_recovery_trigger(to_policy_kind(&policy));
                                    log::warn!(
                                        "[{reaction_id_owned}] Broadcast lag for query '{query_id_clone}': {error_str}"
                                    );
                                    let gap_ctx = BroadcastGapContext {
                                        reaction_id: &reaction_id_owned,
                                        query_id: &query_id_clone,
                                        reaction: &reaction,
                                        query: &query_clone,
                                        policy,
                                        state_store: &state_store_clone,
                                        checkpoints: &checkpoints,
                                        bootstrap_mutex: &bootstrap_mutex,
                                        metrics: &forwarder_metrics,
                                        received_sequence: None,
                                    };
                                    match Self::handle_broadcast_gap(&gap_ctx)
                                    .await
                                    {
                                        Ok(()) => continue,
                                        Err(e) => {
                                            log::error!(
                                                "[{reaction_id_owned}] Recovery failed for broadcast gap on query '{query_id_clone}': {e}"
                                            );
                                            break;
                                        }
                                    }
                                } else {
                                    log::info!(
                                        "[{reaction_id_owned}] Receiver closed for query '{query_id_clone}': {error_str}"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
                .instrument(span),
            );

            abort_handles.push(forwarder_task.abort_handle());
            join_handles.push(forwarder_task);
        }

        // Spawn a supervisor task that monitors all forwarder handles.
        // When ALL forwarders exit, check if the reaction is still Running.
        // Only transition to Error if the exits were unexpected.
        let supervisor_reaction_id = reaction_id.to_string();
        let supervisor_graph = self.graph.clone();

        let supervisor = tokio::spawn(async move {
            // Wait for all forwarder tasks to complete.
            for handle in join_handles {
                if let Err(e) = handle.await {
                    log::warn!("[{supervisor_reaction_id}] Forwarder task failed: {e}");
                }
            }

            // Only transition to Error if the reaction is still Running.
            // If it's already Stopped/Stopping/Error, this was intentional.
            let should_transition = {
                let graph = supervisor_graph.read().await;
                graph
                    .get_component(&supervisor_reaction_id)
                    .map(|n| n.status == ComponentStatus::Running)
                    .unwrap_or(false)
            };

            if should_transition {
                log::warn!(
                    "[{supervisor_reaction_id}] All query subscriptions lost — \
                     transitioning to Error"
                );
                let mut graph = supervisor_graph.write().await;
                let _ = graph.validate_and_transition(
                    &supervisor_reaction_id,
                    ComponentStatus::Error,
                    Some("All query subscriptions lost".to_string()),
                );
            }
        });

        // Store the forwarder abort handles with the supervisor that reaps them.
        self.subscription_tasks.write().await.insert(
            reaction_id.to_string(),
            ReactionSubscriptionTasks {
                forwarder_abort_handles: abort_handles,
                supervisor,
            },
        );

        Ok(())
    }

    async fn enqueue_forwarded_result(
        reaction: &Arc<dyn Reaction>,
        query: &Arc<dyn Query>,
        query_id: &str,
        checkpoints: &Arc<RwLock<HashMap<String, ReactionCheckpoint>>>,
        metrics: &Arc<ReactionMetrics>,
        query_result: Arc<QueryResult>,
    ) -> Result<u64> {
        let sequence = query_result.sequence;
        let result = Arc::try_unwrap(query_result).unwrap_or_else(|result| (*result).clone());
        reaction.enqueue_query_result(result).await?;

        // This checkpoint is process-local. Durable reactions persist their
        // checkpoint only after successful handling of the enqueued result.
        let config_hash = checkpoints
            .read()
            .await
            .get(query_id)
            .map(|checkpoint| checkpoint.config_hash)
            .unwrap_or(0);
        checkpoints.write().await.insert(
            query_id.to_string(),
            ReactionCheckpoint {
                sequence,
                config_hash,
            },
        );
        let query_latest = query
            .output_metrics()
            .map(|metrics| metrics.load_outbox_latest_seq())
            .unwrap_or(sequence);
        metrics.record_checkpoint(sequence, query_latest);
        Ok(sequence)
    }

    /// Handle a broadcast gap (§6): `RecvError::Lagged` in the forwarder loop.
    ///
    /// Applies the reaction's recovery policy:
    /// - `Strict`: return error (forwarder will break)
    /// - `AutoReset`: re-bootstrap from snapshot, update checkpoint (serialized via mutex)
    /// - `AutoSkipGap`: persist only the missing floor before the triggering event
    async fn handle_broadcast_gap(ctx: &BroadcastGapContext<'_>) -> Result<()> {
        let config_hash = crate::queries::compute_config_hash(ctx.query.get_config());

        match ctx.policy {
            ReactionRecoveryPolicy::Strict => Err(anyhow::anyhow!(
                "Strict recovery policy — broadcast lag for query '{}' \
                     is unrecoverable",
                ctx.query_id
            )),
            ReactionRecoveryPolicy::AutoReset => {
                // Serialize bootstrap calls — multiple forwarders may hit gaps concurrently.
                let _guard = ctx.bootstrap_mutex.lock().await;

                log::info!(
                    "[{}] AutoReset on broadcast gap for query '{}'",
                    ctx.reaction_id,
                    ctx.query_id
                );
                ctx.metrics.record_fetch_snapshot();
                let snapshot = ctx.query.fetch_snapshot().await.map_err(|e| {
                    anyhow::anyhow!(
                        "AutoReset broadcast gap: failed to fetch snapshot for '{}': {e}",
                        ctx.query_id
                    )
                })?;

                let cp = ReactionCheckpoint {
                    sequence: snapshot.as_of_sequence,
                    config_hash,
                };

                // Invoke bootstrap hook BEFORE persisting checkpoint — a crash
                // during bootstrap will re-trigger recovery on next start.
                let bootstrap_ctx = BootstrapContext::new(
                    ctx.query_id.to_string(),
                    true,
                    ctx.query.clone(),
                    ctx.reaction_id.to_string(),
                    ctx.state_store.clone(),
                );
                ctx.reaction.bootstrap(bootstrap_ctx).await?;

                // Bootstrap succeeded — now safe to persist checkpoint.
                if let Some(store) = ctx.state_store.as_ref() {
                    crate::reactions::checkpoint::write_checkpoint(
                        store.as_ref(),
                        ctx.reaction_id,
                        ctx.query_id,
                        &cp,
                    )
                    .await?;
                }

                ctx.checkpoints
                    .write()
                    .await
                    .insert(ctx.query_id.to_string(), cp);

                Ok(())
            }
            ReactionRecoveryPolicy::AutoSkipGap => {
                log::info!(
                    "[{}] AutoSkipGap on broadcast gap for query '{}'",
                    ctx.reaction_id,
                    ctx.query_id
                );
                let Some(received_sequence) = ctx.received_sequence else {
                    log::debug!(
                        "[{}] AutoSkipGap for query '{}' is waiting for the next retained \
                         event to determine the exact missing range",
                        ctx.reaction_id,
                        ctx.query_id
                    );
                    return Ok(());
                };
                let skipped_floor = received_sequence.saturating_sub(1);
                let existing_sequence = ctx
                    .checkpoints
                    .read()
                    .await
                    .get(ctx.query_id)
                    .map(|checkpoint| checkpoint.sequence)
                    .unwrap_or(0);
                let skipped_floor = skipped_floor.max(existing_sequence);

                let cp = ReactionCheckpoint {
                    sequence: skipped_floor,
                    config_hash,
                };

                if let Some(store) = ctx.state_store.as_ref() {
                    crate::reactions::checkpoint::write_checkpoint(
                        store.as_ref(),
                        ctx.reaction_id,
                        ctx.query_id,
                        &cp,
                    )
                    .await?;
                }

                ctx.checkpoints
                    .write()
                    .await
                    .insert(ctx.query_id.to_string(), cp);

                Ok(())
            }
        }
    }

    /// Abort all subscription forwarder tasks for a reaction.
    async fn abort_subscription_tasks(&self, reaction_id: &str) {
        Self::abort_subscription_tasks_static(&self.subscription_tasks, reaction_id).await;
    }

    async fn abort_subscription_tasks_static(
        tasks: &Arc<RwLock<HashMap<String, ReactionSubscriptionTasks>>>,
        reaction_id: &str,
    ) {
        let task_set = { tasks.write().await.remove(reaction_id) };
        if let Some(task_set) = task_set {
            for handle in task_set.forwarder_abort_handles {
                handle.abort();
            }
            if let Err(error) = task_set.supervisor.await {
                log::debug!(
                    "[{reaction_id}] Subscription supervisor ended during cleanup: {error}"
                );
            }
        }
    }

    async fn abort_and_reap_forwarders(
        abort_handles: Vec<tokio::task::AbortHandle>,
        join_handles: Vec<tokio::task::JoinHandle<()>>,
    ) {
        for handle in abort_handles {
            handle.abort();
        }
        for handle in join_handles {
            let _ = handle.await;
        }
    }

    /// Get the per-(reaction, query) metrics for a specific reaction.
    ///
    /// Returns a map from query_id to its metrics snapshot.
    ///
    /// # Errors
    /// Returns an error if the reaction does not exist.
    pub async fn get_reaction_metrics(
        &self,
        reaction_id: &str,
    ) -> Result<HashMap<String, crate::metrics::ReactionMetricsSnapshot>> {
        // Verify the reaction exists in the component graph
        let graph = self.graph.read().await;
        if graph
            .get_runtime::<Arc<dyn Reaction>>(reaction_id)
            .is_none()
        {
            return Err(anyhow::anyhow!("Reaction '{reaction_id}' not found",));
        }
        drop(graph);

        let metrics = self.reaction_metrics.read().await;
        Ok(metrics
            .iter()
            .filter(|((rid, _), _)| rid == reaction_id)
            .map(|((_, qid), m)| (qid.clone(), m.snapshot()))
            .collect())
    }

    /// Get the global lifecycle metrics.
    pub fn lifecycle_metrics(&self) -> &Arc<LifecycleMetrics> {
        &self.lifecycle_metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::QuerySubscriptionResponse;
    use crate::component_graph::ComponentStatusHandle;
    use crate::config::schema::QueryConfig;
    use crate::queries::output_state::{OutboxResponse, SnapshotResponse};
    use crate::sources::tests::TestMockSource;
    use async_trait::async_trait;
    use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    // ========================================================================
    // Mock Query for direct unit tests
    // ========================================================================

    /// A mock Query that returns canned snapshot/outbox responses.
    struct MockQuery {
        config: QueryConfig,
        snapshot: tokio::sync::RwLock<SnapshotResponse>,
        outbox_response: tokio::sync::RwLock<Result<OutboxResponse, FetchError>>,
        subscriptions: Mutex<Vec<tokio::sync::mpsc::Sender<Arc<QueryResult>>>>,
    }

    impl MockQuery {
        fn new(config_hash: u64, snapshot_seq: u64) -> Self {
            Self::with_id("q1", config_hash, snapshot_seq)
        }

        fn with_id(id: &str, config_hash: u64, snapshot_seq: u64) -> Self {
            let config = QueryConfig {
                id: id.to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: crate::config::schema::QueryLanguage::Cypher,
                middleware: vec![],
                sources: vec![],
                auto_start: true,
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
                storage_backend: None,
                recovery_policy: None,
                bootstrap_timeout_secs: 300,
                outbox_capacity: 1000,
            };
            let snapshot = SnapshotResponse::new(im::HashMap::new(), snapshot_seq, config_hash);
            Self {
                config,
                snapshot: tokio::sync::RwLock::new(snapshot),
                outbox_response: tokio::sync::RwLock::new(Ok(OutboxResponse {
                    results: vec![],
                    latest_sequence: snapshot_seq,
                    config_hash,
                })),
                subscriptions: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl crate::queries::Query for MockQuery {
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Running
        }
        fn get_config(&self) -> &QueryConfig {
            &self.config
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn subscribe(&self, _reaction_id: String) -> Result<QuerySubscriptionResponse> {
            let (sender, receiver) = tokio::sync::mpsc::channel(16);
            self.subscriptions.lock().await.push(sender);
            Ok(QuerySubscriptionResponse {
                query_id: self.config.id.clone(),
                receiver: Box::new(crate::channels::ChannelChangeReceiver::new(receiver)),
            })
        }
        async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
            Ok(self.snapshot.read().await.clone())
        }
        async fn fetch_outbox(&self, _after_sequence: u64) -> Result<OutboxResponse, FetchError> {
            self.outbox_response.read().await.clone()
        }
    }

    struct MockQueryProvider {
        queries: HashMap<String, Arc<dyn crate::queries::Query>>,
    }

    #[async_trait]
    impl QueryProvider for MockQueryProvider {
        async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn crate::queries::Query>> {
            self.queries
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Query '{id}' not found"))
        }
    }

    struct FailingCheckpointStore {
        inner: crate::state_store::MemoryStateStoreProvider,
        target_reaction: String,
        target_query: String,
        target_sequence: u64,
        remaining_failures: AtomicUsize,
    }

    impl FailingCheckpointStore {
        fn new(
            target_reaction: &str,
            target_query: &str,
            target_sequence: u64,
            failures: usize,
        ) -> Self {
            Self {
                inner: crate::state_store::MemoryStateStoreProvider::new(),
                target_reaction: target_reaction.to_string(),
                target_query: target_query.to_string(),
                target_sequence,
                remaining_failures: AtomicUsize::new(failures),
            }
        }
    }

    #[async_trait]
    impl StateStoreProvider for FailingCheckpointStore {
        async fn get(
            &self,
            store_id: &str,
            key: &str,
        ) -> crate::state_store::StateStoreResult<Option<Vec<u8>>> {
            self.inner.get(store_id, key).await
        }

        async fn set(
            &self,
            store_id: &str,
            key: &str,
            value: Vec<u8>,
        ) -> crate::state_store::StateStoreResult<()> {
            let target_key = format!("checkpoint:{}", self.target_query);
            let should_fail = store_id == self.target_reaction
                && key == target_key
                && bincode::deserialize::<ReactionCheckpoint>(&value)
                    .is_ok_and(|checkpoint| checkpoint.sequence == self.target_sequence)
                && self
                    .remaining_failures
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok();
            if should_fail {
                return Err(crate::state_store::StateStoreError::StorageError(
                    "injected checkpoint write failure".to_string(),
                ));
            }
            self.inner.set(store_id, key, value).await
        }

        async fn delete(
            &self,
            store_id: &str,
            key: &str,
        ) -> crate::state_store::StateStoreResult<bool> {
            self.inner.delete(store_id, key).await
        }

        async fn contains_key(
            &self,
            store_id: &str,
            key: &str,
        ) -> crate::state_store::StateStoreResult<bool> {
            self.inner.contains_key(store_id, key).await
        }

        async fn get_many(
            &self,
            store_id: &str,
            keys: &[&str],
        ) -> crate::state_store::StateStoreResult<HashMap<String, Vec<u8>>> {
            self.inner.get_many(store_id, keys).await
        }

        async fn set_many(
            &self,
            store_id: &str,
            entries: &[(&str, &[u8])],
        ) -> crate::state_store::StateStoreResult<()> {
            self.inner.set_many(store_id, entries).await
        }

        async fn delete_many(
            &self,
            store_id: &str,
            keys: &[&str],
        ) -> crate::state_store::StateStoreResult<usize> {
            self.inner.delete_many(store_id, keys).await
        }

        async fn clear_store(&self, store_id: &str) -> crate::state_store::StateStoreResult<usize> {
            self.inner.clear_store(store_id).await
        }

        async fn list_keys(
            &self,
            store_id: &str,
        ) -> crate::state_store::StateStoreResult<Vec<String>> {
            self.inner.list_keys(store_id).await
        }

        async fn store_exists(&self, store_id: &str) -> crate::state_store::StateStoreResult<bool> {
            self.inner.store_exists(store_id).await
        }

        async fn key_count(&self, store_id: &str) -> crate::state_store::StateStoreResult<usize> {
            self.inner.key_count(store_id).await
        }
    }

    // ========================================================================
    // Configurable mock reaction for testing startup validation and bootstrap
    // ========================================================================

    struct MockReaction {
        id: String,
        queries: Vec<String>,
        durable: bool,
        snapshot_on_fresh: bool,
        policy: ReactionRecoveryPolicy,
        status_handle: ComponentStatusHandle,
        enqueued: Arc<Mutex<Vec<QueryResult>>>,
        bootstrap_count: Arc<AtomicUsize>,
        enqueue_failures: AtomicUsize,
    }

    impl MockReaction {
        fn new(id: &str, queries: Vec<String>) -> Self {
            Self {
                id: id.to_string(),
                queries,
                durable: false,
                snapshot_on_fresh: false,
                policy: ReactionRecoveryPolicy::Strict,
                status_handle: ComponentStatusHandle::new(id),
                enqueued: Arc::new(Mutex::new(Vec::new())),
                bootstrap_count: Arc::new(AtomicUsize::new(0)),
                enqueue_failures: AtomicUsize::new(0),
            }
        }

        fn with_durable(mut self, v: bool) -> Self {
            self.durable = v;
            self
        }

        fn with_snapshot_on_fresh(mut self, v: bool) -> Self {
            self.snapshot_on_fresh = v;
            self
        }

        fn with_policy(mut self, p: ReactionRecoveryPolicy) -> Self {
            self.policy = p;
            self
        }

        fn fail_next_enqueue(&self) {
            self.enqueue_failures.store(1, Ordering::Release);
        }
    }

    #[async_trait]
    impl Reaction for MockReaction {
        fn id(&self) -> &str {
            &self.id
        }
        fn type_name(&self) -> &str {
            "test-mock"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }
        fn auto_start(&self) -> bool {
            false
        }
        async fn initialize(&self, ctx: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(ctx.update_tx.clone()).await;
        }
        async fn start(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Running, None)
                .await;
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Stopped, None)
                .await;
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }
        fn is_durable(&self) -> bool {
            self.durable
        }
        fn needs_snapshot_on_fresh_start(&self) -> bool {
            self.snapshot_on_fresh
        }
        fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
            self.policy
        }
        async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
            if self
                .enqueue_failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                anyhow::bail!("injected reaction enqueue failure");
            }
            self.enqueued.lock().await.push(result);
            Ok(())
        }
        async fn bootstrap(
            &self,
            _ctx: crate::reactions::bootstrap_context::BootstrapContext,
        ) -> Result<()> {
            self.bootstrap_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct QueueBackedReaction {
        base: crate::reactions::common::base::ReactionBase,
    }

    impl QueueBackedReaction {
        fn new(id: &str, queries: Vec<String>) -> Self {
            Self::with_capacity(id, queries, 10_000)
        }

        fn with_capacity(id: &str, queries: Vec<String>, capacity: usize) -> Self {
            Self {
                base: crate::reactions::common::base::ReactionBase::new(
                    crate::reactions::common::base::ReactionBaseParams::new(id, queries)
                        .with_priority_queue_capacity(capacity),
                ),
            }
        }
    }

    #[async_trait]
    impl Reaction for QueueBackedReaction {
        fn id(&self) -> &str {
            self.base.get_id()
        }

        fn type_name(&self) -> &str {
            "queue-backed-test"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_ids(&self) -> Vec<String> {
            self.base.get_queries().to_vec()
        }

        fn auto_start(&self) -> bool {
            false
        }

        async fn initialize(&self, context: crate::context::ReactionRuntimeContext) {
            self.base.initialize(context).await;
        }

        async fn start(&self) -> Result<()> {
            let _shutdown_rx = self.base.create_shutdown_channel().await;
            self.base.set_status(ComponentStatus::Running, None).await;
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
    }

    // ========================================================================
    // Test helpers
    // ========================================================================

    /// Build a DrasiLib with a source and one query (auto_start=false).
    async fn build_core() -> crate::DrasiLib {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        crate::DrasiLib::builder()
            .with_id("test")
            .with_source(source)
            .with_query(
                crate::Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap()
    }

    /// Build a DrasiLib with auto-start query and an injected state store.
    async fn build_core_with_store(
        store: Arc<crate::state_store::MemoryStateStoreProvider>,
    ) -> crate::DrasiLib {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        crate::DrasiLib::builder()
            .with_id("test")
            .with_source(source)
            .with_query(
                crate::Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(true)
                    .build(),
            )
            .with_state_store_provider(store)
            .build()
            .await
            .unwrap()
    }

    /// Helper: create a SourceChange::Insert for a Test node.
    fn make_test_insert(
        source_id: &str,
        node_id: &str,
        val: i32,
    ) -> drasi_core::models::SourceChange {
        let mut props = ElementPropertyMap::default();
        props.insert("val", drasi_core::models::ElementValue::Integer(val.into()));
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, node_id),
                labels: vec!["Test".into()].into(),
                effective_from: 1000,
            },
            properties: props,
        };
        drasi_core::models::SourceChange::Insert { element }
    }

    /// Inject events into a DrasiLib's source via downcast.
    async fn inject_events(core: &crate::DrasiLib, count: usize) {
        let source_arc = core
            .source_manager
            .get_source_instance("src1")
            .await
            .expect("Source 'src1' not found");
        let mock_source = source_arc
            .as_any()
            .downcast_ref::<TestMockSource>()
            .expect("Source is not TestMockSource");
        for i in 0..count {
            let change = make_test_insert("src1", &format!("node_{i}"), i as i32);
            mock_source.inject_event(change).await.unwrap();
        }
        // Give the query time to process events.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    async fn provision_queue_backed_manager(
        queries: HashMap<String, Arc<dyn crate::queries::Query>>,
        state_store: Arc<dyn StateStoreProvider>,
        reaction: QueueBackedReaction,
    ) -> (
        Arc<ReactionManager>,
        Arc<RwLock<crate::component_graph::ComponentGraph>>,
    ) {
        let log_registry = crate::managers::get_or_init_global_registry();
        let (mut graph, mut update_rx) =
            crate::component_graph::ComponentGraph::new("ordering-tests");
        let update_tx = graph.update_sender();
        for query_id in queries.keys() {
            graph.register_query(query_id, HashMap::new(), &[]).unwrap();
        }
        graph
            .register_reaction(reaction.id(), HashMap::new(), &reaction.query_ids())
            .unwrap();
        let graph = Arc::new(RwLock::new(graph));
        let update_graph = graph.clone();
        tokio::spawn(async move {
            while let Some(update) = update_rx.recv().await {
                update_graph.write().await.apply_update(update);
            }
        });

        let manager = Arc::new(ReactionManager::new(
            "ordering-tests",
            log_registry,
            graph.clone(),
            update_tx,
        ));
        manager
            .inject_query_provider(Arc::new(MockQueryProvider { queries }))
            .await;
        manager.inject_state_store(state_store).await;
        manager.provision_reaction(reaction).await.unwrap();
        (manager, graph)
    }

    // ========================================================================
    // §3 Startup validation tests
    // ========================================================================

    #[tokio::test]
    async fn validation_rejects_durable_without_durable_store() {
        let core = build_core().await;
        core.start().await.unwrap();

        // Default store is MemoryStateStoreProvider which is NOT durable.
        let reaction = MockReaction::new("r1", vec!["q1".into()]).with_durable(true);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r1").await;
        assert!(
            result.is_err(),
            "Expected error for durable reaction without durable store"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("durable"),
            "Error should mention 'durable': {msg}"
        );
    }

    #[tokio::test]
    async fn validation_rejects_snapshot_with_auto_skip_gap() {
        let core = build_core().await;
        core.start().await.unwrap();

        let reaction = MockReaction::new("r2", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::AutoSkipGap);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r2").await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("AutoSkipGap") || msg.contains("incompatible"),
            "Error should mention incompatibility: {msg}"
        );
    }

    #[tokio::test]
    async fn validation_rejects_no_snapshot_with_auto_reset() {
        let core = build_core().await;
        core.start().await.unwrap();

        let reaction = MockReaction::new("r3", vec!["q1".into()])
            .with_snapshot_on_fresh(false)
            .with_policy(ReactionRecoveryPolicy::AutoReset);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r3").await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("AutoReset") || msg.contains("incompatible"),
            "Error should mention incompatibility: {msg}"
        );
    }

    #[tokio::test]
    async fn validation_allows_non_durable_no_snapshot_strict() {
        let core = build_core().await;
        core.start().await.unwrap();

        let reaction = MockReaction::new("r4", vec!["q1".into()]);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r4").await;
        assert!(result.is_ok(), "Expected success: {:?}", result.err());
    }

    // ========================================================================
    // Fresh start tests
    // ========================================================================

    #[tokio::test]
    async fn fresh_start_no_snapshot_starts_at_seq_zero() {
        let core = build_core().await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();

        let reaction = MockReaction::new("r5", vec!["q1".into()]);
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r5").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r5",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = core.get_reaction_status("r5").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);
    }

    // ========================================================================
    // §5 Config-hash mismatch + outbox catch-up tests
    // ========================================================================

    #[tokio::test]
    async fn config_hash_mismatch_with_auto_reset_triggers_bootstrap() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Pre-write a checkpoint with a WRONG config hash.
        let wrong_cp = ReactionCheckpoint {
            sequence: 0,
            config_hash: 99999,
        };
        crate::reactions::checkpoint::write_checkpoint(store.as_ref(), "r_reset", "q1", &wrong_cp)
            .await
            .unwrap();

        // Add reaction with AutoReset + needs_snapshot.
        let reaction = MockReaction::new("r_reset", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::AutoReset);
        let bc = reaction.bootstrap_count.clone();
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r_reset").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r_reset",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Verify: bootstrap was called (AutoReset re-bootstraps).
        assert!(
            bc.load(Ordering::SeqCst) > 0,
            "Expected bootstrap() to be called on config-hash mismatch with AutoReset"
        );

        // Verify: checkpoint should now have the correct config hash.
        let cp = crate::reactions::checkpoint::read_checkpoint(store.as_ref(), "r_reset", "q1")
            .await
            .unwrap();
        assert!(cp.is_some(), "Checkpoint should exist after AutoReset");
        assert_ne!(
            cp.unwrap().config_hash,
            99999,
            "Checkpoint hash should have been updated from the wrong value"
        );
    }

    #[tokio::test]
    async fn config_hash_mismatch_with_strict_fails_startup() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Pre-write checkpoint with wrong hash.
        let wrong_cp = ReactionCheckpoint {
            sequence: 0,
            config_hash: 99999,
        };
        crate::reactions::checkpoint::write_checkpoint(store.as_ref(), "r_strict", "q1", &wrong_cp)
            .await
            .unwrap();

        // Strict policy should reject on hash mismatch.
        let reaction = MockReaction::new("r_strict", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::Strict);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r_strict").await;
        assert!(
            result.is_err(),
            "Strict policy should fail on config-hash mismatch"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Strict") || msg.contains("manual"),
            "Error should mention Strict policy: {msg}"
        );
    }

    #[tokio::test]
    async fn outbox_catchup_replays_entries_to_reaction() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Inject data so the query has outbox entries.
        inject_events(&core, 3).await;

        // Get the correct config hash for this query.
        let query_arc = core.query_manager.get_query_instance("q1").await.unwrap();
        let config_hash = crate::queries::compute_config_hash(query_arc.get_config());

        // Pre-write a checkpoint with the CORRECT hash but seq=0 (behind the outbox).
        let old_cp = ReactionCheckpoint {
            sequence: 0,
            config_hash,
        };
        crate::reactions::checkpoint::write_checkpoint(store.as_ref(), "r_catchup", "q1", &old_cp)
            .await
            .unwrap();

        // Start reaction — should catch up via outbox.
        let reaction = MockReaction::new("r_catchup", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::AutoReset);
        let enqueued = reaction.enqueued.clone();
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r_catchup").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r_catchup",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Verify: outbox entries were replayed to the reaction.
        let results = enqueued.lock().await;
        assert!(
            results.len() >= 3,
            "Expected at least 3 outbox entries replayed, got {}",
            results.len()
        );

        // Verify: checkpoint should have advanced.
        let cp = crate::reactions::checkpoint::read_checkpoint(store.as_ref(), "r_catchup", "q1")
            .await
            .unwrap()
            .expect("Checkpoint should exist");
        assert!(
            cp.sequence > 0,
            "Checkpoint sequence should have advanced from 0, got {}",
            cp.sequence
        );
    }

    // ========================================================================
    // §6 handle_broadcast_gap — direct unit tests
    // ========================================================================

    #[tokio::test]
    async fn broadcast_gap_strict_returns_error() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 10));
        let reaction: Arc<dyn Reaction> = Arc::new(MockReaction::new("r1", vec!["q1".into()]));
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction,
            query: &query,
            policy: ReactionRecoveryPolicy::Strict,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
            received_sequence: None,
        })
        .await;

        assert!(result.is_err(), "Strict policy should return error on gap");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("Strict"), "Error should mention Strict: {msg}");
        assert!(
            checkpoints.read().await.is_empty(),
            "No checkpoint should be set on Strict error"
        );
    }

    #[tokio::test]
    async fn broadcast_gap_auto_reset_fetches_snapshot_and_bootstraps() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_snapshot_on_fresh(true)
                .with_policy(ReactionRecoveryPolicy::AutoReset),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction_trait,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
            received_sequence: None,
        })
        .await;

        assert!(
            result.is_ok(),
            "AutoReset should succeed: {:?}",
            result.err()
        );

        // Checkpoint should be set to snapshot sequence.
        let cps = checkpoints.read().await;
        let cp = cps.get("q1").expect("Checkpoint for q1 should exist");
        assert_eq!(
            cp.sequence, 100,
            "Checkpoint should match snapshot sequence"
        );

        // Config hash should be computed from the MockQuery's config.
        let expected_hash = crate::queries::compute_config_hash(query.get_config());
        assert_eq!(cp.config_hash, expected_hash);

        // Bootstrap should have been called exactly once.
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            1,
            "bootstrap() should have been called once"
        );
    }

    #[tokio::test]
    async fn broadcast_gap_auto_skip_gap_skips_only_missing_range() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 50));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_policy(ReactionRecoveryPolicy::AutoSkipGap),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction_trait,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoSkipGap,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
            received_sequence: Some(42),
        })
        .await;

        assert!(
            result.is_ok(),
            "AutoSkipGap should succeed: {:?}",
            result.err()
        );

        // The triggering event (42) is still deliverable, so only its missing
        // predecessor range may be checkpointed as skipped.
        let cps = checkpoints.read().await;
        let cp = cps.get("q1").expect("Checkpoint for q1 should exist");
        assert_eq!(cp.sequence, 41);

        // bootstrap() should NOT be called for AutoSkipGap.
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            0,
            "bootstrap() should not be called for AutoSkipGap"
        );
    }

    #[tokio::test]
    async fn startup_auto_skip_gap_handles_empty_and_saturated_bounds() {
        let empty_query = Arc::new(MockQuery::new(42, 0));
        let empty_query_trait: Arc<dyn crate::queries::Query> = empty_query.clone();
        let reaction: Arc<dyn Reaction> = Arc::new(MockReaction::new("r1", vec!["q1".into()]));
        let checkpoint = ReactionCheckpoint {
            sequence: 0,
            config_hash: 42,
        };
        let metrics = Arc::new(ReactionMetrics::new());
        let empty = ReactionManager::replay_retained_outbox_after_gap(
            "r1",
            "q1",
            &checkpoint,
            &reaction,
            &empty_query_trait,
            &None,
            &metrics,
            &crate::queries::OutboxGap {
                requested: 0,
                earliest_available: 0,
                latest_sequence: 0,
                config_hash: 42,
            },
        )
        .await
        .expect("empty outbox bounds should not underflow");
        assert_eq!(empty.sequence, 0);

        let saturated_query = Arc::new(MockQuery::new(42, u64::MAX));
        *saturated_query.outbox_response.write().await = Ok(OutboxResponse {
            results: vec![Arc::new(QueryResult::new(
                "q1".to_string(),
                u64::MAX,
                chrono::Utc::now(),
                Vec::new(),
                HashMap::new(),
            ))],
            latest_sequence: u64::MAX,
            config_hash: 42,
        });
        let saturated_query_trait: Arc<dyn crate::queries::Query> = saturated_query;
        let saturated = ReactionManager::replay_retained_outbox_after_gap(
            "r1",
            "q1",
            &ReactionCheckpoint {
                sequence: u64::MAX - 2,
                config_hash: 42,
            },
            &reaction,
            &saturated_query_trait,
            &None,
            &metrics,
            &crate::queries::OutboxGap {
                requested: u64::MAX - 2,
                earliest_available: u64::MAX,
                latest_sequence: u64::MAX,
                config_hash: 42,
            },
        )
        .await
        .expect("saturated outbox bounds should not overflow");
        assert_eq!(saturated.sequence, u64::MAX);
    }

    #[tokio::test]
    async fn retained_outbox_then_live_is_ordered_by_reaction_base() {
        let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let query = Arc::new(MockQuery::new(42, 6));
        *query.outbox_response.write().await = Ok(OutboxResponse {
            results: vec![
                Arc::new(QueryResult::new(
                    "q1".to_string(),
                    5,
                    timestamp + chrono::Duration::seconds(10),
                    Vec::new(),
                    HashMap::new(),
                )),
                Arc::new(QueryResult::new(
                    "q1".to_string(),
                    6,
                    timestamp,
                    Vec::new(),
                    HashMap::new(),
                )),
            ],
            latest_sequence: 6,
            config_hash: 42,
        });
        let query_trait: Arc<dyn crate::queries::Query> = query;
        let reaction = Arc::new(QueueBackedReaction::new("r1", vec!["q1".to_string()]));
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let metrics = Arc::new(ReactionMetrics::new());

        let recovered = ReactionManager::replay_retained_outbox_after_gap(
            "r1",
            "q1",
            &ReactionCheckpoint {
                sequence: 4,
                config_hash: 42,
            },
            &reaction_trait,
            &query_trait,
            &None,
            &metrics,
            &crate::queries::OutboxGap {
                requested: 4,
                earliest_available: 5,
                latest_sequence: 6,
                config_hash: 42,
            },
        )
        .await
        .expect("retained outbox replay should enqueue sequences 5 and 6");
        assert_eq!(recovered.sequence, 6);

        let checkpoints = Arc::new(RwLock::new(HashMap::from([("q1".to_string(), recovered)])));
        ReactionManager::enqueue_forwarded_result(
            &reaction_trait,
            &query_trait,
            "q1",
            &checkpoints,
            &metrics,
            Arc::new(QueryResult::new(
                "q1".to_string(),
                7,
                timestamp - chrono::Duration::seconds(10),
                Vec::new(),
                HashMap::new(),
            )),
        )
        .await
        .expect("buffered live result should enqueue after retained replay");

        let mut sequences = Vec::new();
        for _ in 0..3 {
            sequences.push(reaction.base.priority_queue.dequeue().await.sequence);
        }
        assert_eq!(sequences, vec![5, 6, 7]);
    }

    #[tokio::test]
    async fn failed_catchup_generations_are_clean_and_retryable() {
        let reaction_id = "retry-ordering";
        let store = Arc::new(FailingCheckpointStore::new(reaction_id, "q1", 10, 2));
        let q1 = Arc::new(MockQuery::with_id("q1", 0, 10));
        let q2 = Arc::new(MockQuery::with_id("q2", 0, 4));
        let q1_hash = crate::queries::compute_config_hash(q1.get_config());
        let q2_hash = crate::queries::compute_config_hash(q2.get_config());
        let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();

        *q1.outbox_response.write().await = Ok(OutboxResponse {
            results: (5..=10)
                .map(|sequence| {
                    Arc::new(QueryResult::new(
                        "q1".to_string(),
                        sequence,
                        timestamp - chrono::Duration::seconds(sequence as i64),
                        Vec::new(),
                        HashMap::new(),
                    ))
                })
                .collect(),
            latest_sequence: 10,
            config_hash: q1_hash,
        });
        *q2.outbox_response.write().await = Ok(OutboxResponse {
            results: (1..=4)
                .map(|sequence| {
                    Arc::new(QueryResult::new(
                        "q2".to_string(),
                        sequence,
                        timestamp + chrono::Duration::seconds(sequence as i64),
                        Vec::new(),
                        HashMap::new(),
                    ))
                })
                .collect(),
            latest_sequence: 4,
            config_hash: q2_hash,
        });
        crate::reactions::checkpoint::write_checkpoint(
            store.as_ref(),
            reaction_id,
            "q1",
            &ReactionCheckpoint {
                sequence: 4,
                config_hash: q1_hash,
            },
        )
        .await
        .unwrap();
        crate::reactions::checkpoint::write_checkpoint(
            store.as_ref(),
            reaction_id,
            "q2",
            &ReactionCheckpoint {
                sequence: 0,
                config_hash: q2_hash,
            },
        )
        .await
        .unwrap();

        let reaction =
            QueueBackedReaction::new(reaction_id, vec!["q1".to_string(), "q2".to_string()]);
        let base = reaction.base.clone_shared();
        let queries: HashMap<String, Arc<dyn crate::queries::Query>> = HashMap::from([
            ("q1".to_string(), q1 as Arc<dyn crate::queries::Query>),
            ("q2".to_string(), q2 as Arc<dyn crate::queries::Query>),
        ]);
        let (manager, graph) =
            provision_queue_backed_manager(queries, store.clone(), reaction).await;

        for _ in 0..2 {
            let error = manager
                .start_reaction(reaction_id.to_string())
                .await
                .expect_err("injected checkpoint write should fail startup");
            assert!(
                format!("{error:#}").contains("injected checkpoint write failure"),
                "unexpected startup error: {error:#}"
            );
            assert_eq!(
                graph
                    .read()
                    .await
                    .get_component(reaction_id)
                    .unwrap()
                    .status,
                ComponentStatus::Error
            );
            assert_eq!(base.priority_queue.depth().await, 0);
            assert_eq!(
                crate::reactions::checkpoint::read_checkpoint(store.as_ref(), reaction_id, "q1")
                    .await
                    .unwrap()
                    .unwrap()
                    .sequence,
                4,
                "failed generation must not advance the durable checkpoint"
            );
        }

        manager
            .start_reaction(reaction_id.to_string())
            .await
            .expect("third generation should replay from checkpoint 4");

        let depth = base.priority_queue.depth().await;
        assert_eq!(depth, 10);
        let mut by_query: HashMap<String, Vec<u64>> = HashMap::new();
        for _ in 0..depth {
            let result = base.priority_queue.try_dequeue().await.unwrap();
            by_query
                .entry(result.query_id.clone())
                .or_default()
                .push(result.sequence);
        }
        assert_eq!(by_query["q1"], vec![5, 6, 7, 8, 9, 10]);
        assert_eq!(by_query["q2"], vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn stop_cancels_full_queue_bootstrap_and_restart_succeeds() {
        let reaction_id = "stop-full-bootstrap";
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let query = Arc::new(MockQuery::with_id("q1", 0, 6));
        let config_hash = crate::queries::compute_config_hash(query.get_config());
        let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        *query.outbox_response.write().await = Ok(OutboxResponse {
            results: vec![
                Arc::new(QueryResult::new(
                    "q1".to_string(),
                    5,
                    timestamp,
                    Vec::new(),
                    HashMap::new(),
                )),
                Arc::new(QueryResult::new(
                    "q1".to_string(),
                    6,
                    timestamp,
                    Vec::new(),
                    HashMap::new(),
                )),
            ],
            latest_sequence: 6,
            config_hash,
        });
        crate::reactions::checkpoint::write_checkpoint(
            store.as_ref(),
            reaction_id,
            "q1",
            &ReactionCheckpoint {
                sequence: 4,
                config_hash,
            },
        )
        .await
        .unwrap();

        let reaction = QueueBackedReaction::with_capacity(reaction_id, vec!["q1".to_string()], 1);
        let base = reaction.base.clone_shared();
        let queries: HashMap<String, Arc<dyn crate::queries::Query>> = HashMap::from([(
            "q1".to_string(),
            query.clone() as Arc<dyn crate::queries::Query>,
        )]);
        let (manager, _graph) =
            provision_queue_backed_manager(queries, store.clone(), reaction).await;

        let start_manager = manager.clone();
        let start_task =
            tokio::spawn(
                async move { start_manager.start_reaction(reaction_id.to_string()).await },
            );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while base.priority_queue.metrics().await.blocked_enqueue_count == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bootstrap did not block on the full reaction queue");

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            manager.stop_reaction(reaction_id.to_string()),
        )
        .await
        .expect("stop deadlocked with a blocked bootstrap enqueue")
        .expect("stop failed");
        let start_error = tokio::time::timeout(std::time::Duration::from_secs(1), start_task)
            .await
            .expect("bootstrap producer did not exit after stop")
            .expect("start task panicked")
            .expect_err("stopped bootstrap generation must not report success");
        assert!(
            format!("{start_error:#}").contains("priority queue is closed"),
            "unexpected start error: {start_error:#}"
        );
        assert_eq!(base.priority_queue.depth().await, 0);
        assert_eq!(
            crate::reactions::checkpoint::read_checkpoint(store.as_ref(), reaction_id, "q1")
                .await
                .unwrap()
                .unwrap()
                .sequence,
            4
        );

        *query.outbox_response.write().await = Ok(OutboxResponse {
            results: vec![Arc::new(QueryResult::new(
                "q1".to_string(),
                5,
                timestamp,
                Vec::new(),
                HashMap::new(),
            ))],
            latest_sequence: 5,
            config_hash,
        });
        manager
            .start_reaction(reaction_id.to_string())
            .await
            .expect("restart should reopen a clean reaction queue generation");
        assert_eq!(base.priority_queue.depth().await, 1);
        assert_eq!(base.priority_queue.try_dequeue().await.unwrap().sequence, 5);
    }

    #[tokio::test]
    async fn auto_skip_gap_persists_missing_floor_not_moving_query_latest() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction: Arc<dyn Reaction> = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_policy(ReactionRecoveryPolicy::AutoSkipGap),
        );
        let store: Arc<dyn StateStoreProvider> =
            Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let state_store = Some(store.clone());
        let checkpoints = Arc::new(RwLock::new(HashMap::from([(
            "q1".to_string(),
            ReactionCheckpoint {
                sequence: 10,
                config_hash: 42,
            },
        )])));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoSkipGap,
            state_store: &state_store,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
            received_sequence: Some(42),
        })
        .await
        .expect("AutoSkipGap should persist the missing floor");

        assert_eq!(checkpoints.read().await["q1"].sequence, 41);
        assert_eq!(
            crate::reactions::checkpoint::read_checkpoint(store.as_ref(), "r1", "q1")
                .await
                .expect("read persisted skipped floor")
                .expect("persisted skipped floor")
                .sequence,
            41,
            "query latest 100 must not checkpoint deliverable results 42..=100"
        );
    }

    #[tokio::test]
    async fn auto_skip_gap_waits_for_retained_event_before_advancing() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction: Arc<dyn Reaction> = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_policy(ReactionRecoveryPolicy::AutoSkipGap),
        );
        let checkpoints = Arc::new(RwLock::new(HashMap::from([(
            "q1".to_string(),
            ReactionCheckpoint {
                sequence: 10,
                config_hash: 42,
            },
        )])));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoSkipGap,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
            received_sequence: None,
        })
        .await
        .expect("lag notification alone should not advance a checkpoint");

        assert_eq!(checkpoints.read().await["q1"].sequence, 10);
    }

    #[tokio::test]
    async fn auto_skip_gap_enqueue_failure_keeps_current_event_recoverable() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_policy(ReactionRecoveryPolicy::AutoSkipGap),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::from([(
            "q1".to_string(),
            ReactionCheckpoint {
                sequence: 10,
                config_hash: 42,
            },
        )])));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));
        let metrics = Arc::new(ReactionMetrics::new());

        ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction_trait,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoSkipGap,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &metrics,
            received_sequence: Some(42),
        })
        .await
        .expect("skip missing range");
        reaction.fail_next_enqueue();
        let current = Arc::new(QueryResult::new(
            "q1".to_string(),
            42,
            chrono::Utc::now(),
            Vec::new(),
            HashMap::new(),
        ));
        assert!(ReactionManager::enqueue_forwarded_result(
            &reaction_trait,
            &query,
            "q1",
            &checkpoints,
            &metrics,
            current.clone(),
        )
        .await
        .is_err());
        assert_eq!(
            checkpoints.read().await["q1"].sequence,
            41,
            "failed enqueue must leave the current event above the checkpoint"
        );
        assert!(reaction.enqueued.lock().await.is_empty());

        ReactionManager::enqueue_forwarded_result(
            &reaction_trait,
            &query,
            "q1",
            &checkpoints,
            &metrics,
            current,
        )
        .await
        .expect("restart/retry should deliver the current event");
        assert_eq!(checkpoints.read().await["q1"].sequence, 42);
        assert_eq!(reaction.enqueued.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn auto_skip_gap_repeated_episodes_advance_only_after_delivery() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_policy(ReactionRecoveryPolicy::AutoSkipGap),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::from([(
            "q1".to_string(),
            ReactionCheckpoint {
                sequence: 1,
                config_hash: 42,
            },
        )])));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));
        let metrics = Arc::new(ReactionMetrics::new());

        for received_sequence in [10, 20] {
            ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
                reaction_id: "r1",
                query_id: "q1",
                reaction: &reaction_trait,
                query: &query,
                policy: ReactionRecoveryPolicy::AutoSkipGap,
                state_store: &None,
                checkpoints: &checkpoints,
                bootstrap_mutex: &bootstrap_mutex,
                metrics: &metrics,
                received_sequence: Some(received_sequence),
            })
            .await
            .expect("skip one missing range");
            assert_eq!(
                checkpoints.read().await["q1"].sequence,
                received_sequence - 1
            );
            ReactionManager::enqueue_forwarded_result(
                &reaction_trait,
                &query,
                "q1",
                &checkpoints,
                &metrics,
                Arc::new(QueryResult::new(
                    "q1".to_string(),
                    received_sequence,
                    chrono::Utc::now(),
                    Vec::new(),
                    HashMap::new(),
                )),
            )
            .await
            .expect("deliver retained gap-triggering event");
            assert_eq!(checkpoints.read().await["q1"].sequence, received_sequence);
        }
        assert_eq!(
            reaction
                .enqueued
                .lock()
                .await
                .iter()
                .map(|result| result.sequence)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
    }

    // ========================================================================
    // Forwarder sequence-filtering test
    // ========================================================================

    #[tokio::test]
    async fn forwarder_filters_stale_events_after_gate_opens() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Inject initial data so the query has events at seq > 0.
        inject_events(&core, 5).await;

        // Start reaction — it bootstraps and creates a checkpoint at the current seq.
        let reaction = MockReaction::new("r_filter", vec!["q1".into()])
            .with_snapshot_on_fresh(false)
            .with_policy(ReactionRecoveryPolicy::AutoSkipGap);
        let enqueued = reaction.enqueued.clone();
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r_filter").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r_filter",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Clear any results that may have arrived during startup.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        enqueued.lock().await.clear();

        // Push NEW data after the reaction is running — only these should be forwarded.
        inject_events(&core, 3).await;

        // Verify: only the new events should be enqueued (not the 5 old ones).
        let results = enqueued.lock().await;
        assert_eq!(
            results.len(),
            3,
            "Expected exactly 3 new events, got {} (stale events should be filtered)",
            results.len()
        );
    }

    // ========================================================================
    // §7 Bootstrap failure and concurrent gap recovery tests
    // ========================================================================

    /// MockQuery variant whose `fetch_snapshot` returns an error.
    struct FailingMockQuery {
        config: QueryConfig,
    }

    impl FailingMockQuery {
        fn new() -> Self {
            Self {
                config: QueryConfig {
                    id: "q1".to_string(),
                    query: "MATCH (n) RETURN n".to_string(),
                    query_language: crate::config::schema::QueryLanguage::Cypher,
                    middleware: vec![],
                    sources: vec![],
                    auto_start: true,
                    joins: None,
                    enable_bootstrap: true,
                    bootstrap_buffer_size: 10000,
                    priority_queue_capacity: None,
                    dispatch_buffer_capacity: None,
                    dispatch_mode: None,
                    storage_backend: None,
                    recovery_policy: None,
                    bootstrap_timeout_secs: 300,
                    outbox_capacity: 1000,
                },
            }
        }
    }

    #[async_trait]
    impl crate::queries::Query for FailingMockQuery {
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Running
        }
        fn get_config(&self) -> &QueryConfig {
            &self.config
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn subscribe(&self, _reaction_id: String) -> Result<QuerySubscriptionResponse> {
            Err(anyhow::anyhow!("not supported"))
        }
        async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
            Err(FetchError::NotRunning {
                status: ComponentStatus::Error,
            })
        }
        async fn fetch_outbox(&self, _after_sequence: u64) -> Result<OutboxResponse, FetchError> {
            Ok(OutboxResponse {
                results: vec![],
                latest_sequence: 0,
                config_hash: 0,
            })
        }
    }

    /// Test: AutoReset bootstrap fails because snapshot fetch errors.
    /// The handle_broadcast_gap should propagate the error without leaving
    /// a stale checkpoint in the map.
    #[tokio::test]
    async fn auto_reset_bootstrap_failure_propagates_error() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(FailingMockQuery::new());
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_snapshot_on_fresh(true)
                .with_policy(ReactionRecoveryPolicy::AutoReset),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction_trait,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
            received_sequence: None,
        })
        .await;

        // Should fail because fetch_snapshot returns error.
        assert!(
            result.is_err(),
            "AutoReset should fail when snapshot fetch errors"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("failed to fetch snapshot"),
            "Error should mention snapshot failure: {msg}"
        );

        // No checkpoint should be written on failure.
        assert!(
            checkpoints.read().await.is_empty(),
            "No checkpoint should be set when bootstrap fails"
        );

        // bootstrap() should NOT have been called (failed before reaching it).
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            0,
            "bootstrap() should not be called when snapshot fetch fails"
        );
    }

    /// MockReaction variant whose `bootstrap()` always returns an error.
    struct FailingBootstrapReaction {
        id: String,
        queries: Vec<String>,
        policy: ReactionRecoveryPolicy,
        status_handle: ComponentStatusHandle,
        enqueued: Arc<Mutex<Vec<QueryResult>>>,
        bootstrap_count: Arc<AtomicUsize>,
    }

    impl FailingBootstrapReaction {
        fn new(id: &str, queries: Vec<String>) -> Self {
            Self {
                id: id.to_string(),
                queries,
                policy: ReactionRecoveryPolicy::AutoReset,
                status_handle: ComponentStatusHandle::new(id),
                enqueued: Arc::new(Mutex::new(Vec::new())),
                bootstrap_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Reaction for FailingBootstrapReaction {
        fn id(&self) -> &str {
            &self.id
        }
        fn type_name(&self) -> &str {
            "test-failing-bootstrap"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }
        fn auto_start(&self) -> bool {
            false
        }
        async fn initialize(&self, ctx: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(ctx.update_tx.clone()).await;
        }
        async fn start(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Running, None)
                .await;
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Stopped, None)
                .await;
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }
        fn is_durable(&self) -> bool {
            false
        }
        fn needs_snapshot_on_fresh_start(&self) -> bool {
            true
        }
        fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
            self.policy
        }
        async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
            self.enqueued.lock().await.push(result);
            Ok(())
        }
        async fn bootstrap(
            &self,
            _ctx: crate::reactions::bootstrap_context::BootstrapContext,
        ) -> Result<()> {
            self.bootstrap_count.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("Simulated bootstrap failure"))
        }
    }

    /// Test: AutoReset with successful snapshot fetch but bootstrap() hook failure.
    /// The error should propagate and no checkpoint should be persisted.
    #[tokio::test]
    async fn auto_reset_bootstrap_hook_failure_propagates_error() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(FailingBootstrapReaction::new("r1", vec!["q1".into()]));
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ReactionManager::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction_trait,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
            received_sequence: None,
        })
        .await;

        // Should fail because bootstrap() returns error.
        assert!(result.is_err(), "Should fail when bootstrap hook errors");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Simulated bootstrap failure"),
            "Error should contain bootstrap failure message: {msg}"
        );

        // No checkpoint should be persisted on bootstrap failure.
        assert!(
            checkpoints.read().await.is_empty(),
            "No checkpoint should be set when bootstrap hook fails"
        );

        // bootstrap() was called once (and failed).
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            1,
            "bootstrap() should have been called once before failing"
        );
    }

    /// Test: Concurrent multi-query gap recovery is serialized by the bootstrap mutex.
    /// Two concurrent AutoReset gap recoveries should not interleave bootstrap calls.
    #[tokio::test]
    async fn concurrent_gap_recovery_serialized_by_mutex() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into(), "q2".into()])
                .with_snapshot_on_fresh(true)
                .with_policy(ReactionRecoveryPolicy::AutoReset),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        // Launch two concurrent gap recoveries for different query IDs.
        let metrics_q1 = Arc::new(ReactionMetrics::new());
        let metrics_q2 = Arc::new(ReactionMetrics::new());
        let ctx_q1 = BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            reaction: &reaction_trait,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &metrics_q1,
            received_sequence: None,
        };
        let ctx_q2 = BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q2",
            reaction: &reaction_trait,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &metrics_q2,
            received_sequence: None,
        };
        let (r1, r2) = tokio::join!(
            ReactionManager::handle_broadcast_gap(&ctx_q1),
            ReactionManager::handle_broadcast_gap(&ctx_q2),
        );

        assert!(
            r1.is_ok(),
            "First gap recovery should succeed: {:?}",
            r1.err()
        );
        assert!(
            r2.is_ok(),
            "Second gap recovery should succeed: {:?}",
            r2.err()
        );

        // Both checkpoints should be set.
        let cps = checkpoints.read().await;
        assert!(cps.contains_key("q1"), "Checkpoint for q1 should exist");
        assert!(cps.contains_key("q2"), "Checkpoint for q2 should exist");

        // Bootstrap should have been called exactly twice (once per query, serialized).
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            2,
            "bootstrap() should be called exactly twice (serialized by mutex)"
        );
    }
}
