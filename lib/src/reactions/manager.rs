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
use crate::queries::output_state::FetchError;
use crate::queries::Query;
use crate::reactions::bootstrap_context::BootstrapContext;
use crate::reactions::checkpoint::ReactionCheckpoint;
use crate::reactions::snapshot_fetcher::InProcessSnapshotFetcher;
use crate::reactions::{QueryProvider, Reaction};
use crate::recovery::ReactionRecoveryPolicy;
use crate::state_store::StateStoreProvider;

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
    /// Abort handles to subscription forwarder + supervisor tasks per reaction
    subscription_tasks: Arc<RwLock<HashMap<String, Vec<tokio::task::AbortHandle>>>>,
    /// Shared component graph — the single source of truth for component metadata,
    /// state, relationships, runtime instances, AND event history.
    graph: Arc<RwLock<ComponentGraph>>,
    /// Channel sender for routing status updates through the graph update loop.
    /// Managers send transitional states (Starting, Stopping, Reconfiguring) here;
    /// the loop applies them to the graph and records events automatically.
    update_tx: ComponentUpdateSender,
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

        crate::managers::lifecycle_helpers::start_component(
            &self.graph,
            &id,
            "reaction",
            &reaction,
        )
        .await?;

        // Create the bootstrap gate — forwarders wait on this before processing.
        // Using a watch channel (not Notify) so late subscribers see the current value
        // and cannot miss the notification.
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);

        if let Err(e) = self
            .subscribe_and_bootstrap(&id, reaction.clone(), gate_rx)
            .await
        {
            // Abort any forwarder/supervisor tasks spawned during wire_subscriptions.
            Self::abort_subscription_tasks_static(&self.subscription_tasks, &id).await;

            // Revert to Error since the reaction can't receive data without subscriptions
            let mut graph = self.graph.write().await;
            let _ = graph.validate_and_transition(
                &id,
                ComponentStatus::Error,
                Some(format!("Bootstrap failed: {e}")),
            );
            return Err(e);
        }

        // Open the gate — forwarders begin draining buffered events.
        let _ = gate_tx.send(true);

        Ok(())
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
                    return Err(anyhow::anyhow!(
                        "Reaction '{}' requires a durable state store (is_durable=true), \
                         but no state store is configured",
                        reaction.id()
                    ));
                }
                Some(s) if !s.is_durable() => {
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
            return Err(anyhow::anyhow!(
                "Reaction '{}': needs_snapshot_on_fresh_start=true is incompatible with \
                 AutoSkipGap recovery policy (cannot skip gap if snapshot is required)",
                reaction.id()
            ));
        }

        // Rule 3: !snapshot + AutoReset is contradictory
        if !needs_snapshot && policy == ReactionRecoveryPolicy::AutoReset {
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
    ) -> Result<ReactionCheckpoint> {
        let config_hash = crate::queries::compute_config_hash(query.get_config());

        if reaction.needs_snapshot_on_fresh_start() {
            info!("[{reaction_id}] Fresh start for query '{query_id}' — fetching snapshot");
            let snapshot = query.fetch_snapshot().await.map_err(|e| {
                anyhow::anyhow!("Failed to fetch snapshot for query '{query_id}': {e}")
            })?;

            let cp = ReactionCheckpoint {
                sequence: snapshot.as_of_sequence,
                config_hash,
            };

            // Persist the checkpoint.
            if let Some(store) = state_store.as_ref() {
                crate::reactions::checkpoint::write_checkpoint(
                    store.as_ref(),
                    reaction_id,
                    query_id,
                    &cp,
                )
                .await?;
            } else {
                log::debug!(
                    "[{reaction_id}] No state store — checkpoint for query '{query_id}' \
                     not persisted (seq={})",
                    cp.sequence
                );
            }

            bootstrap_queries.push((query_id.to_string(), query.clone()));
            Ok(cp)
        } else {
            // No snapshot needed — just record the current sequence.
            // If the query is not running or hasn't bootstrapped yet, start from 0.
            let seq = match query.fetch_outbox(0).await {
                Ok(resp) => resp.latest_sequence,
                Err(FetchError::OutboxGap(gap)) => gap.latest_sequence,
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
    ) -> Result<ReactionCheckpoint> {
        match query.fetch_outbox(checkpoint.sequence).await {
            Ok(outbox_resp) => {
                // Replay outbox entries by enqueuing them.
                // Track the last successfully enqueued sequence to avoid
                // advancing the checkpoint past failed entries.
                let mut last_ok_seq = checkpoint.sequence;
                for entry in &outbox_resp.results {
                    let result = (*entry).as_ref().clone();
                    match reaction.enqueue_query_result(result).await {
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
                    reaction,
                    query,
                    policy,
                    state_store,
                    bootstrap_queries,
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
    async fn apply_recovery_policy(
        &self,
        reaction_id: &str,
        query_id: &str,
        reaction: &Arc<dyn Reaction>,
        query: &Arc<dyn Query>,
        policy: ReactionRecoveryPolicy,
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        bootstrap_queries: &mut Vec<(String, Arc<dyn Query>)>,
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
                let snapshot = query.fetch_snapshot().await.map_err(|e| {
                    anyhow::anyhow!(
                        "AutoReset: failed to fetch snapshot for query '{query_id}': {e}"
                    )
                })?;

                let cp = ReactionCheckpoint {
                    sequence: snapshot.as_of_sequence,
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

                bootstrap_queries.push((query_id.to_string(), query.clone()));
                Ok(cp)
            }
            ReactionRecoveryPolicy::AutoSkipGap => {
                info!(
                    "[{reaction_id}] AutoSkipGap for query '{query_id}' — \
                     jumping to current sequence"
                );

                // Get current sequence from the query.
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

        // Abort subscription forwarder tasks before stopping
        self.abort_subscription_tasks(&id).await;

        crate::managers::lifecycle_helpers::stop_component(&self.graph, &id, "reaction", &reaction)
            .await
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
            let query = query_provider.get_query_instance(query_id).await?;

            let subscription = query.subscribe(reaction_id.to_string()).await?;
            let mut receiver = subscription.receiver;

            let reaction = reaction.clone();
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

                    log::debug!(
                        "[{reaction_id_owned}] Started result forwarder for query '{query_id_clone}' \
                         (initial_seq={initial_seq})"
                    );

                    loop {
                        match receiver.recv().await {
                            Ok(query_result) => {
                                // Skip events already covered by the bootstrap snapshot/outbox catchup.
                                if query_result.sequence <= initial_seq {
                                    continue;
                                }
                                let result = Arc::try_unwrap(query_result)
                                    .unwrap_or_else(|arc| (*arc).clone());
                                if let Err(e) = reaction.enqueue_query_result(result).await {
                                    log::error!(
                                        "[{reaction_id_owned}] Failed to enqueue result from query '{query_id_clone}': {e}"
                                    );
                                }
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                if error_str.contains("lagged") {
                                    // §6: Broadcast gap detected.
                                    log::warn!(
                                        "[{reaction_id_owned}] Broadcast lag for query '{query_id_clone}': {error_str}"
                                    );
                                    match Self::handle_broadcast_gap(
                                        &reaction_id_owned,
                                        &query_id_clone,
                                        &reaction,
                                        &query_clone,
                                        policy,
                                        &state_store_clone,
                                        &checkpoints,
                                        &bootstrap_mutex,
                                    )
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
    async fn handle_broadcast_gap(
        reaction_id: &str,
        query_id: &str,
        reaction: &Arc<dyn Reaction>,
        query: &Arc<dyn Query>,
        policy: ReactionRecoveryPolicy,
        state_store: &Option<Arc<dyn StateStoreProvider>>,
        checkpoints: &Arc<RwLock<HashMap<String, ReactionCheckpoint>>>,
        bootstrap_mutex: &Arc<tokio::sync::Mutex<()>>,
    ) -> Result<()> {
        let config_hash = crate::queries::compute_config_hash(query.get_config());

        match policy {
            ReactionRecoveryPolicy::Strict => Err(anyhow::anyhow!(
                "Strict recovery policy — broadcast lag for query '{query_id}' \
                     is unrecoverable"
            )),
            ReactionRecoveryPolicy::AutoReset => {
                // Serialize bootstrap calls — multiple forwarders may hit gaps concurrently.
                let _guard = bootstrap_mutex.lock().await;

                log::info!("[{reaction_id}] AutoReset on broadcast gap for query '{query_id}'");
                let snapshot = query.fetch_snapshot().await.map_err(|e| {
                    anyhow::anyhow!(
                        "AutoReset broadcast gap: failed to fetch snapshot for '{query_id}': {e}"
                    )
                })?;

                let cp = ReactionCheckpoint {
                    sequence: snapshot.as_of_sequence,
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

                checkpoints.write().await.insert(query_id.to_string(), cp);

                // Invoke bootstrap hook for this query.
                let ctx = BootstrapContext::new(
                    query_id.to_string(),
                    true,
                    query.clone(),
                    reaction_id.to_string(),
                    state_store.clone(),
                );
                reaction.bootstrap(ctx).await?;

                Ok(())
            }
            ReactionRecoveryPolicy::AutoSkipGap => {
                log::info!("[{reaction_id}] AutoSkipGap on broadcast gap for query '{query_id}'");
                let current_seq = match query.fetch_outbox(0).await {
                    Ok(resp) => resp.latest_sequence,
                    Err(FetchError::OutboxGap(gap)) => gap.latest_sequence,
                    Err(FetchError::NotRunning { .. } | FetchError::TimedOut) => {
                        // Query not running — fall back to existing checkpoint sequence.
                        let existing_seq = checkpoints
                            .read()
                            .await
                            .get(query_id)
                            .map(|cp| cp.sequence)
                            .unwrap_or(0);
                        log::info!(
                            "[{reaction_id}] AutoSkipGap: query '{query_id}' not running, \
                             keeping checkpoint at seq={existing_seq}"
                        );
                        existing_seq
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

                checkpoints.write().await.insert(query_id.to_string(), cp);

                Ok(())
            }
        }
    }

    /// Abort all subscription forwarder tasks for a reaction.
    async fn abort_subscription_tasks(&self, reaction_id: &str) {
        Self::abort_subscription_tasks_static(&self.subscription_tasks, reaction_id).await;
    }

    async fn abort_subscription_tasks_static(
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
    }

    impl MockQuery {
        fn new(config_hash: u64, snapshot_seq: u64) -> Self {
            let config = QueryConfig {
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
            Err(anyhow::anyhow!("MockQuery does not support subscribe"))
        }
        async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
            Ok(self.snapshot.read().await.clone())
        }
        async fn fetch_outbox(&self, _after_sequence: u64) -> Result<OutboxResponse, FetchError> {
            self.outbox_response.read().await.clone()
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

        let result = ReactionManager::handle_broadcast_gap(
            "r1",
            "q1",
            &reaction,
            &query,
            ReactionRecoveryPolicy::Strict,
            &None,
            &checkpoints,
            &bootstrap_mutex,
        )
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

        let result = ReactionManager::handle_broadcast_gap(
            "r1",
            "q1",
            &reaction_trait,
            &query,
            ReactionRecoveryPolicy::AutoReset,
            &None,
            &checkpoints,
            &bootstrap_mutex,
        )
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
    async fn broadcast_gap_auto_skip_gap_jumps_to_current_seq() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 50));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_policy(ReactionRecoveryPolicy::AutoSkipGap),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ReactionManager::handle_broadcast_gap(
            "r1",
            "q1",
            &reaction_trait,
            &query,
            ReactionRecoveryPolicy::AutoSkipGap,
            &None,
            &checkpoints,
            &bootstrap_mutex,
        )
        .await;

        assert!(
            result.is_ok(),
            "AutoSkipGap should succeed: {:?}",
            result.err()
        );

        // Checkpoint should jump to current sequence.
        let cps = checkpoints.read().await;
        let cp = cps.get("q1").expect("Checkpoint for q1 should exist");
        assert_eq!(cp.sequence, 50, "Checkpoint should jump to latest sequence");

        // bootstrap() should NOT be called for AutoSkipGap.
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            0,
            "bootstrap() should not be called for AutoSkipGap"
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
}
