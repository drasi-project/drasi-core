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

use anyhow::Result;
use log::{info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::channels::*;
use crate::component_graph::ComponentGraph;
use crate::metrics::{LifecycleMetrics, ReactionMetrics, RecoveryPolicyKind};
use crate::queries::output_state::FetchError;
use crate::queries::Query;
use crate::reactions::bootstrap_context::BootstrapContext;
use crate::reactions::checkpoint::ReactionCheckpoint;
use crate::reactions::QueryProvider;
use crate::recovery::ReactionRecoveryPolicy;
use crate::state_store::StateStoreProvider;

use super::QueryConsumer;

/// Key for per-consumer per-query metrics: `(consumer_id, query_id)`.
pub type MetricsKey = (String, String);

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
pub(crate) struct BroadcastGapContext<'a> {
    pub(crate) reaction_id: &'a str,
    pub(crate) query_id: &'a str,
    pub(crate) consumer: &'a Arc<dyn QueryConsumer>,
    pub(crate) query: &'a Arc<dyn Query>,
    pub(crate) policy: ReactionRecoveryPolicy,
    pub(crate) state_store: &'a Option<Arc<dyn StateStoreProvider>>,
    pub(crate) checkpoints: &'a Arc<RwLock<HashMap<String, ReactionCheckpoint>>>,
    pub(crate) bootstrap_mutex: &'a Arc<tokio::sync::Mutex<()>>,
    pub(crate) metrics: &'a Arc<ReactionMetrics>,
}

#[derive(Clone)]
pub struct ConsumptionEngine {
    instance_id: String,
    query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    subscription_tasks: Arc<RwLock<HashMap<String, Vec<tokio::task::AbortHandle>>>>,
    graph: Arc<RwLock<ComponentGraph>>,
    metrics: Arc<RwLock<HashMap<MetricsKey, Arc<ReactionMetrics>>>>,
    lifecycle_metrics: Arc<LifecycleMetrics>,
}

impl ConsumptionEngine {
    pub fn new(
        instance_id: impl Into<String>,
        graph: Arc<RwLock<ComponentGraph>>,
        query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
        state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
        subscription_tasks: Arc<RwLock<HashMap<String, Vec<tokio::task::AbortHandle>>>>,
        metrics: Arc<RwLock<HashMap<MetricsKey, Arc<ReactionMetrics>>>>,
        lifecycle_metrics: Arc<LifecycleMetrics>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            query_provider,
            state_store,
            subscription_tasks,
            graph,
            metrics,
            lifecycle_metrics,
        }
    }

    pub fn metrics(&self) -> &Arc<RwLock<HashMap<MetricsKey, Arc<ReactionMetrics>>>> {
        &self.metrics
    }

    pub fn lifecycle_metrics(&self) -> &Arc<LifecycleMetrics> {
        &self.lifecycle_metrics
    }

    pub fn subscription_tasks(
        &self,
    ) -> &Arc<RwLock<HashMap<String, Vec<tokio::task::AbortHandle>>>> {
        &self.subscription_tasks
    }

    pub async fn inject_query_provider(&self, qp: Arc<dyn QueryProvider>) {
        *self.query_provider.write().await = Some(qp);
    }

    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
    }

    /// Wire live subscriptions, then perform per-query bootstrap.
    ///
    /// 1. Wire subscriptions first (forwarders wait on gate, events buffer in broadcast channel)
    /// 2. Read checkpoints from state store
    /// 3. For each query, run the per-query startup flowchart (§5)
    /// 4. Invoke `consumer.catch_up()` if any query needed a full reset
    ///
    /// The bootstrap gate is NOT opened here — the caller opens it after this returns.
    pub async fn subscribe_and_bootstrap(
        &self,
        reaction_id: &str,
        consumer: Arc<dyn QueryConsumer>,
        gate: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let query_ids = consumer.consumed_query_ids();
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
        let policy = consumer.default_recovery_policy();

        // Shared checkpoint map — populated during bootstrap, read by forwarders after gate opens.
        let shared_checkpoints: Arc<RwLock<HashMap<String, ReactionCheckpoint>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // 1. Wire subscriptions FIRST so events buffer in broadcast channels
        //    while bootstrap runs. Forwarders wait on gate before processing.
        self.wire_subscriptions(
            reaction_id,
            &consumer,
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
                let mut metrics_map = self.metrics.write().await;
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
                            &consumer,
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
                                &consumer,
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
                                &consumer,
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
                consumer.catch_up(ctx).await?;
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
        consumer: &Arc<dyn QueryConsumer>,
        query: &Arc<dyn Query>,
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        bootstrap_queries: &mut Vec<(String, Arc<dyn Query>)>,
        metrics: &Arc<ReactionMetrics>,
    ) -> Result<ReactionCheckpoint> {
        let config_hash = crate::queries::compute_config_hash(query.get_config());

        if consumer.needs_snapshot_on_fresh_start() {
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
                        let mut last_ok_seq = 0u64;
                        for entry in &resp.results {
                            let result = (*entry).as_ref().clone();
                            match consumer.enqueue_query_result(result).await {
                                Ok(()) => last_ok_seq = entry.sequence,
                                Err(e) => {
                                    warn!(
                                        "[{reaction_id}] Failed to replay outbox entry for query \
                                         '{query_id}' seq={}: {e}",
                                        entry.sequence
                                    );
                                    break;
                                }
                            }
                        }
                        if last_ok_seq != resp.latest_sequence {
                            info!(
                                "[{reaction_id}] Partial outbox replay for query '{query_id}' — \
                                 replayed up to seq={last_ok_seq}, latest_seq={}",
                                resp.latest_sequence
                            );
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
        consumer: &Arc<dyn QueryConsumer>,
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
                    match consumer.enqueue_query_result(result).await {
                        Ok(()) => last_ok_seq = entry.sequence,
                        Err(e) => {
                            warn!(
                                "[{reaction_id}] Failed to replay outbox entry for query \
                                 '{query_id}' seq={}: {e}",
                                entry.sequence
                            );
                            break;
                        }
                    }
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
            Err(FetchError::OutboxGap(_gap)) => {
                info!(
                    "[{reaction_id}] Outbox gap for query '{query_id}' — applying recovery policy"
                );
                self.apply_recovery_policy(
                    reaction_id,
                    query_id,
                    consumer,
                    query,
                    policy,
                    state_store,
                    bootstrap_queries,
                    metrics,
                )
                .await
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

    /// Apply the recovery policy when a gap or hash mismatch is detected.
    #[allow(clippy::too_many_arguments)]
    async fn apply_recovery_policy(
        &self,
        reaction_id: &str,
        query_id: &str,
        _consumer: &Arc<dyn QueryConsumer>,
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

    /// Wire live broadcast subscriptions with gap detection (§6) and bootstrap gate.
    ///
    /// For each query:
    /// 1. Subscribe to the query's result broadcast channel
    /// 2. Spawn a forwarder task that waits on the bootstrap gate, then drains events
    /// 3. On `RecvError::Lagged`, apply the recovery policy
    async fn wire_subscriptions(
        &self,
        reaction_id: &str,
        consumer: &Arc<dyn QueryConsumer>,
        query_provider: &Arc<dyn QueryProvider>,
        query_ids: &[String],
        shared_checkpoints: Arc<RwLock<HashMap<String, ReactionCheckpoint>>>,
        gate: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let instance_id = self.instance_id.clone();
        let policy = consumer.default_recovery_policy();
        let state_store = self.state_store.read().await.clone();
        let mut abort_handles: Vec<tokio::task::AbortHandle> = Vec::new();
        let mut join_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // Mutex to serialize concurrent bootstrap calls (§9 — multi-query gap recovery).
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        for query_id in query_ids {
            let query = query_provider.get_query_instance(query_id).await?;

            let subscription = query.subscribe(reaction_id.to_string()).await?;
            let mut receiver = subscription.receiver;

            // Create or retrieve per-(reaction, query) metrics
            let metrics_key = (reaction_id.to_string(), query_id.clone());
            let forwarder_metrics = {
                let mut metrics_map = self.metrics.write().await;
                metrics_map
                    .entry(metrics_key)
                    .or_insert_with(|| Arc::new(ReactionMetrics::new()))
                    .clone()
            };

            let consumer = consumer.clone();
            let query_id_clone = query_id.clone();
            let reaction_id_owned = reaction_id.to_string();
            let mut gate_rx = gate.clone();
            let query_clone = query.clone();
            let state_store_clone = state_store.clone();
            let checkpoints = shared_checkpoints.clone();
            let bootstrap_mutex = bootstrap_mutex.clone();

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
                                // Skip events already covered by the bootstrap snapshot/outbox catchup.
                                if query_result.sequence <= initial_seq {
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
                                if last_forwarded_seq > 0
                                    && query_result.sequence > last_forwarded_seq.saturating_add(1)
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
                                        consumer: &consumer,
                                        query: &query_clone,
                                        policy,
                                        state_store: &state_store_clone,
                                        checkpoints: &checkpoints,
                                        bootstrap_mutex: &bootstrap_mutex,
                                        metrics: &forwarder_metrics,
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

                                last_forwarded_seq = query_result.sequence;
                                let seq = query_result.sequence;
                                let result = Arc::try_unwrap(query_result)
                                    .unwrap_or_else(|arc| (*arc).clone());
                                if let Err(e) = consumer.enqueue_query_result(result).await {
                                    log::error!(
                                        "[{reaction_id_owned}] Failed to enqueue result from query '{query_id_clone}': {e}"
                                    );
                                } else {
                                    // Advance in-memory checkpoint so forwarder tracks
                                    // position (skip stale events on reconnect).
                                    // NOTE: We do NOT persist to durable storage here.
                                    // The reaction should persist its checkpoint after
                                    // successfully processing the event. This prevents
                                    // permanent skipping if the process crashes between
                                    // enqueue and handler processing.
                                    let config_hash = {
                                        let cps = checkpoints.read().await;
                                        cps.get(&query_id_clone).map(|cp| cp.config_hash).unwrap_or(0)
                                    };
                                    let cp = ReactionCheckpoint { sequence: seq, config_hash };
                                    checkpoints.write().await.insert(query_id_clone.clone(), cp);

                                    // Update reaction metrics: checkpoint position
                                    let query_latest = query_clone
                                        .output_metrics()
                                        .map(|m| m.load_outbox_latest_seq())
                                        .unwrap_or(seq);
                                    forwarder_metrics.record_checkpoint(seq, query_latest);
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
                                        consumer: &consumer,
                                        query: &query_clone,
                                        policy,
                                        state_store: &state_store_clone,
                                        checkpoints: &checkpoints,
                                        bootstrap_mutex: &bootstrap_mutex,
                                        metrics: &forwarder_metrics,
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

        abort_handles.push(supervisor.abort_handle());

        // Store abort handles so stop_reaction/teardown can cancel everything.
        self.subscription_tasks
            .write()
            .await
            .insert(reaction_id.to_string(), abort_handles);

        Ok(())
    }

    /// Handle a broadcast gap (§6): `RecvError::Lagged` in the forwarder loop.
    ///
    /// Applies the reaction's recovery policy:
    /// - `Strict`: return error (forwarder will break)
    /// - `AutoReset`: re-bootstrap from snapshot, update checkpoint (serialized via mutex)
    /// - `AutoSkipGap`: jump to current sequence, update checkpoint
    pub(crate) async fn handle_broadcast_gap(ctx: &BroadcastGapContext<'_>) -> Result<()> {
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
                ctx.consumer.catch_up(bootstrap_ctx).await?;

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
                ctx.metrics.record_fetch_outbox();
                let current_seq = match ctx.query.fetch_outbox(0).await {
                    Ok(resp) => resp.latest_sequence,
                    Err(FetchError::OutboxGap(gap)) => gap.latest_sequence,
                    Err(FetchError::NotRunning { .. } | FetchError::TimedOut) => {
                        // Query not running — fall back to existing checkpoint sequence.
                        let existing_seq = ctx
                            .checkpoints
                            .read()
                            .await
                            .get(ctx.query_id)
                            .map(|cp| cp.sequence)
                            .unwrap_or(0);
                        log::info!(
                            "[{}] AutoSkipGap: query '{}' not running, \
                             keeping checkpoint at seq={existing_seq}",
                            ctx.reaction_id,
                            ctx.query_id
                        );
                        existing_seq
                    }
                };

                let cp = ReactionCheckpoint {
                    sequence: current_seq,
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
    pub async fn abort_subscription_tasks(&self, reaction_id: &str) {
        Self::abort_subscription_tasks_static(&self.subscription_tasks, reaction_id).await;
    }

    pub async fn abort_subscription_tasks_static(
        tasks: &Arc<RwLock<HashMap<String, Vec<tokio::task::AbortHandle>>>>,
        reaction_id: &str,
    ) {
        if let Some(handles) = tasks.write().await.remove(reaction_id) {
            for handle in handles {
                handle.abort();
            }
        }
    }
}
