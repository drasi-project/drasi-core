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

//! Fixed query pipeline host.
//!
//! `DrasiQuery` owns construction and source subscription setup. This host owns
//! bootstrap supervision, durable output reconciliation, and live processing.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Instant, SystemTime},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    evaluation::context::QueryPartEvaluationContext,
    interface::{
        AtomicResultTransaction, CheckpointStore, CreatedIndexes, IndexError, LiveResultsWriter,
        OutboxWriter, RowMutation, SessionControl, SessionGuard,
    },
    models::SourceChange,
    query::ContinuousQuery,
};
use futures::StreamExt;
use log::{debug, error, info, warn};
use tokio::{
    sync::{oneshot, Mutex as AsyncMutex, RwLock},
    task::JoinHandle,
};
use tracing::Instrument;

use super::{PriorityQueue, QueryBase, QueryOutputState, SequenceDedup};
use crate::{
    channels::{
        BootstrapEventReceiver, ChangeDispatcher, ComponentStatus, QueryResult, ResultDiff,
        SourceControl, SourceEvent, SourceEventWrapper,
    },
    component_graph::ComponentStatusHandle,
    metrics::QueryOutputMetrics,
    profiling::ProfilingMetadata,
    sources::FutureQueueSource,
};

/// Versioned private marker stored through the existing checkpoint store.
///
/// A NUL-prefixed identifier is rejected as a configured source id by
/// `DrasiQuery`, so it cannot collide with a real source checkpoint.
pub(super) const QUERY_BOOTSTRAP_MARKER_V1: &str = "\0drasi:query-bootstrap:v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueryBootstrapRecoveryState {
    Absent,
    InProgress,
    Complete,
}

pub(super) struct QueryBootstrapInput {
    source_id: String,
    receiver: BootstrapEventReceiver,
    result_receiver: Option<oneshot::Receiver<anyhow::Result<crate::bootstrap::BootstrapResult>>>,
}

impl QueryBootstrapInput {
    pub(super) fn new(
        source_id: String,
        receiver: BootstrapEventReceiver,
        result_receiver: Option<
            oneshot::Receiver<anyhow::Result<crate::bootstrap::BootstrapResult>>,
        >,
    ) -> Self {
        Self {
            source_id,
            receiver,
            result_receiver,
        }
    }
}

/// Shared owner for every task that can enqueue into one query's priority queue.
///
/// Fatal atomic processing, normal stop, and restart preparation all use this
/// single serialized cleanup path. The processor task itself is intentionally
/// not stored here, so it can fence ingress without aborting itself.
#[derive(Clone)]
pub(super) struct QueryIngressFence {
    subscription_tasks: Arc<RwLock<Vec<JoinHandle<()>>>>,
    priority_queue: PriorityQueue,
    cleanup_lock: Arc<AsyncMutex<()>>,
}

struct AbortTasksOnDrop {
    tasks: Vec<JoinHandle<()>>,
}

impl AbortTasksOnDrop {
    fn new(tasks: Vec<JoinHandle<()>>) -> Self {
        Self { tasks }
    }

    fn take(&mut self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.tasks)
    }
}

impl Drop for AbortTasksOnDrop {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl QueryIngressFence {
    pub(super) fn new(priority_queue: PriorityQueue) -> Self {
        Self {
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            priority_queue,
            cleanup_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    pub(super) async fn install(&self, tasks: Vec<JoinHandle<()>>) {
        let mut pending = AbortTasksOnDrop::new(tasks);
        let _cleanup = self.cleanup_lock.lock().await;
        let mut old_tasks = AbortTasksOnDrop::new({
            let mut installed = self.subscription_tasks.write().await;
            let old_tasks = installed.drain(..).collect();
            *installed = pending.take();
            old_tasks
        });
        Self::abort_and_reap(old_tasks.take()).await;
    }

    pub(super) async fn push(&self, task: JoinHandle<()>) {
        let mut pending = AbortTasksOnDrop::new(vec![task]);
        let _cleanup = self.cleanup_lock.lock().await;
        self.subscription_tasks
            .write()
            .await
            .push(pending.take().pop().expect("pending query ingress task"));
    }

    pub(super) async fn close(&self) {
        let _cleanup = self.cleanup_lock.lock().await;
        let tasks = {
            let mut installed = self.subscription_tasks.write().await;
            installed.drain(..).collect()
        };
        Self::abort_and_reap(tasks).await;

        let drained = self.priority_queue.drain().await.len();
        if drained > 0 {
            debug!("Discarded {drained} queued events while fencing query ingress");
        }
    }

    async fn abort_and_reap(tasks: Vec<JoinHandle<()>>) {
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    pub(super) async fn task_count(&self) -> usize {
        self.subscription_tasks.read().await.len()
    }
}

/// In-process fence for an output that committed durably but was not applied
/// to the host's in-memory output state.
#[derive(Clone, Default)]
pub(super) struct AtomicPublicationRecovery {
    pending: Arc<Mutex<Option<Arc<QueryResult>>>>,
}

impl AtomicPublicationRecovery {
    pub(super) fn is_required(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn require(&self, result: Arc<QueryResult>) {
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
    }

    fn pending_result(&self) -> Option<Arc<QueryResult>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn reconcile_in_memory(&self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

struct CommittedPublicationGuard {
    recovery: AtomicPublicationRecovery,
    armed: bool,
}

impl CommittedPublicationGuard {
    fn new(recovery: AtomicPublicationRecovery, has_output: bool) -> Self {
        debug_assert!(!has_output || recovery.is_required());
        Self {
            recovery,
            armed: has_output,
        }
    }

    fn complete(mut self) {
        if self.armed {
            self.recovery.reconcile_in_memory();
            self.armed = false;
        }
    }
}

impl Drop for CommittedPublicationGuard {
    fn drop(&mut self) {
        debug_assert!(!self.armed || self.recovery.is_required());
    }
}

/// Dependencies that define the lifecycle and ingress boundary of the fixed query host.
pub(super) struct QueryHostRuntime {
    instance_id: String,
    query_id: String,
    priority_queue: PriorityQueue,
    status_handle: ComponentStatusHandle,
    future_queue_source: Arc<FutureQueueSource>,
    ingress_fence: QueryIngressFence,
}

impl QueryHostRuntime {
    pub(super) fn new(
        instance_id: String,
        query_id: String,
        priority_queue: PriorityQueue,
        status_handle: ComponentStatusHandle,
        future_queue_source: Arc<FutureQueueSource>,
        ingress_fence: QueryIngressFence,
    ) -> Self {
        Self {
            instance_id,
            query_id,
            priority_queue,
            status_handle,
            future_queue_source,
            ingress_fence,
        }
    }
}

/// Dependencies for processing live source changes and acknowledging durable progress.
pub(super) struct QueryLiveDependencies {
    continuous_query: Arc<ContinuousQuery>,
    checkpoint_store: Arc<dyn CheckpointStore>,
    checkpoint_sequences: HashMap<String, u64>,
    position_handles: HashMap<String, Arc<AtomicU64>>,
}

impl QueryLiveDependencies {
    pub(super) fn new(
        continuous_query: Arc<ContinuousQuery>,
        checkpoint_store: Arc<dyn CheckpointStore>,
        checkpoint_sequences: HashMap<String, u64>,
        position_handles: HashMap<String, Arc<AtomicU64>>,
    ) -> Self {
        Self {
            continuous_query,
            checkpoint_store,
            checkpoint_sequences,
            position_handles,
        }
    }
}

/// Explicit processing capability for the fixed query host.
///
/// `Atomic` owns the validated token and the exact writer instances used to
/// mint it. Missing or mismatched bundles remain `Legacy`; the host never
/// infers atomicity from an individual writer.
#[derive(Clone)]
pub(super) enum QueryProcessingMode {
    Atomic(AtomicQueryResources),
    Legacy,
}

#[derive(Clone)]
pub(super) struct AtomicQueryResources {
    transaction: AtomicResultTransaction,
    session_control: Arc<dyn SessionControl>,
    checkpoint_store: Arc<dyn CheckpointStore>,
    outbox_writer: Arc<dyn OutboxWriter>,
    live_results_writer: Arc<dyn LiveResultsWriter>,
}

impl QueryProcessingMode {
    pub(super) const fn legacy() -> Self {
        Self::Legacy
    }

    /// Capture the bundle-level capability before `CreatedIndexes` is distributed.
    pub(super) fn from_created_indexes(created: &CreatedIndexes) -> Self {
        let Some(transaction) = created.atomic_result_transaction() else {
            return Self::Legacy;
        };

        let (Some(checkpoint_store), Some(outbox_writer), Some(live_results_writer)) = (
            created.checkpoint_store.as_ref(),
            created.outbox_writer.as_ref(),
            created.live_results_writer.as_ref(),
        ) else {
            error!(
                "Atomic result capability was issued without every persistent output writer; \
                 falling back to legacy processing"
            );
            return Self::Legacy;
        };

        Self::Atomic(AtomicQueryResources {
            transaction,
            session_control: created.set.session_control.clone(),
            checkpoint_store: checkpoint_store.clone(),
            outbox_writer: outbox_writer.clone(),
            live_results_writer: live_results_writer.clone(),
        })
    }

    pub(super) const fn diagnostic_name(&self) -> &'static str {
        match self {
            Self::Atomic(_) => "atomic",
            Self::Legacy => "legacy",
        }
    }
}

/// Deterministic crate-private seam for transaction-boundary tests.
#[async_trait]
pub(super) trait QueryProcessingObserver: Send + Sync {
    async fn after_output_staged(&self) -> std::result::Result<(), IndexError> {
        Ok(())
    }

    async fn after_commit_before_publish(&self) -> Result<()> {
        Ok(())
    }

    fn after_source_acknowledged(&self) {}
}

/// Dependencies for preparing and publishing query output.
pub(super) struct QueryOutputDependencies {
    query_id: String,
    output_state: Arc<RwLock<QueryOutputState>>,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
    outbox_writer: Option<Arc<dyn OutboxWriter>>,
    live_results_writer: Option<Arc<dyn LiveResultsWriter>>,
    checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    outbox_capacity: usize,
    output_metrics: Arc<QueryOutputMetrics>,
    observer: Option<Arc<dyn QueryProcessingObserver>>,
    publication_recovery: AtomicPublicationRecovery,
}

impl QueryOutputDependencies {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        query_id: String,
        output_state: Arc<RwLock<QueryOutputState>>,
        dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
        outbox_writer: Option<Arc<dyn OutboxWriter>>,
        live_results_writer: Option<Arc<dyn LiveResultsWriter>>,
        checkpoint_store: Option<Arc<dyn CheckpointStore>>,
        outbox_capacity: usize,
        output_metrics: Arc<QueryOutputMetrics>,
    ) -> Self {
        Self {
            query_id,
            output_state,
            dispatchers,
            outbox_writer,
            live_results_writer,
            checkpoint_store,
            outbox_capacity,
            output_metrics,
            observer: None,
            publication_recovery: AtomicPublicationRecovery::default(),
        }
    }

    pub(super) fn with_publication_recovery(
        mut self,
        publication_recovery: AtomicPublicationRecovery,
    ) -> Self {
        self.publication_recovery = publication_recovery;
        self
    }

    #[cfg(test)]
    pub(super) fn with_observer(mut self, observer: Arc<dyn QueryProcessingObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

/// Host-owned bootstrap lifecycle for one query start.
pub(super) struct QueryBootstrapScope {
    inputs: Vec<QueryBootstrapInput>,
    checkpoint_store: Arc<dyn CheckpointStore>,
    session_control: Option<Arc<dyn SessionControl>>,
}

impl QueryBootstrapScope {
    pub(super) fn new(
        inputs: Vec<QueryBootstrapInput>,
        checkpoint_store: Arc<dyn CheckpointStore>,
        session_control: Option<Arc<dyn SessionControl>>,
    ) -> Self {
        Self {
            inputs,
            checkpoint_store,
            session_control,
        }
    }

    pub(super) async fn read_recovery_state(
        checkpoint_store: &dyn CheckpointStore,
    ) -> Result<QueryBootstrapRecoveryState> {
        let marker = checkpoint_store
            .read_checkpoint(QUERY_BOOTSTRAP_MARKER_V1)
            .await
            .context("failed to read the query bootstrap recovery marker")?;
        match marker.map(|checkpoint| checkpoint.sequence) {
            None => Ok(QueryBootstrapRecoveryState::Absent),
            Some(0) => Ok(QueryBootstrapRecoveryState::InProgress),
            Some(1) => Ok(QueryBootstrapRecoveryState::Complete),
            Some(sequence) => {
                anyhow::bail!("invalid query bootstrap recovery marker sequence {sequence}")
            }
        }
    }

    async fn run(
        &mut self,
        instance_id: &str,
        query_id: &str,
        continuous_query: Arc<ContinuousQuery>,
        mode: QueryProcessingMode,
        output: Arc<OutputPublicationStage>,
    ) -> Result<Vec<String>> {
        if self.inputs.is_empty() {
            info!("Query '{query_id}' has no bootstrap channels");
            return Ok(Vec::new());
        }

        info!(
            "Query '{query_id}' starting bootstrap from {} sources",
            self.inputs.len()
        );
        if self.checkpoint_store.is_persistent() {
            self.persist_checkpoints(&[(QUERY_BOOTSTRAP_MARKER_V1, 0, None)])
                .await
                .context("failed to persist the bootstrap in-progress marker")?;
        } else {
            output.reset_bootstrap_state().await;
        }
        output
            .dispatch_bootstrap_control("bootstrapStarted", Some(self.inputs.len()))
            .await;

        let processing_lock = Arc::new(AsyncMutex::new(()));
        let mut tasks = futures::stream::FuturesUnordered::new();
        let mut aborts = PendingBootstrapTaskAborts::default();
        for input in std::mem::take(&mut self.inputs) {
            let source_id = input.source_id.clone();
            let task = tokio::spawn(
                process_bootstrap_source(
                    query_id.to_string(),
                    input,
                    continuous_query.clone(),
                    mode.clone(),
                    output.clone(),
                    processing_lock.clone(),
                )
                .instrument(tracing::info_span!(
                    "query_bootstrap",
                    instance_id = %instance_id,
                    component_id = %query_id,
                    component_type = "query",
                    source_id = %source_id,
                )),
            );
            aborts.push(task.abort_handle());
            tasks.push(task);
        }

        let mut handoffs = Vec::new();
        while let Some(joined) = tasks.next().await {
            let handoff = joined
                .context("query bootstrap task failed to join")?
                .context("query bootstrap source failed")?;
            handoffs.push(handoff);
        }

        let checkpoints: Vec<(&str, u64, Option<&Bytes>)> = handoffs
            .iter()
            .map(|handoff| {
                (
                    handoff.source_id.as_str(),
                    0,
                    handoff.source_position.as_ref(),
                )
            })
            .chain(self.checkpoint_store.is_persistent().then_some((
                QUERY_BOOTSTRAP_MARKER_V1,
                1,
                None,
            )))
            .collect();
        self.persist_checkpoints(&checkpoints)
            .await
            .context("failed to persist the final bootstrap handoff")?;

        output
            .dispatch_bootstrap_control("bootstrapCompleted", None)
            .await;
        info!("[BOOTSTRAP] Query '{query_id}' bootstrap completed");

        Ok(handoffs
            .into_iter()
            .map(|handoff| handoff.source_id)
            .collect())
    }

    async fn persist_checkpoints(&self, checkpoints: &[(&str, u64, Option<&Bytes>)]) -> Result<()> {
        let session = match &self.session_control {
            Some(session_control) => Some(
                SessionGuard::begin(session_control.clone())
                    .await
                    .context("failed to begin bootstrap checkpoint transaction")?,
            ),
            None => None,
        };

        for (source_id, sequence, source_position) in checkpoints {
            self.checkpoint_store
                .stage_checkpoint(source_id, *sequence, *source_position)
                .await
                .with_context(|| {
                    format!("failed to stage bootstrap checkpoint for '{source_id}'")
                })?;
        }

        if let Some(session) = session {
            session
                .commit()
                .await
                .context("failed to commit bootstrap checkpoints")?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct PendingBootstrapTaskAborts {
    handles: Vec<tokio::task::AbortHandle>,
}

impl PendingBootstrapTaskAborts {
    fn push(&mut self, handle: tokio::task::AbortHandle) {
        self.handles.push(handle);
    }
}

impl Drop for PendingBootstrapTaskAborts {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

struct BootstrapSourceHandoff {
    source_id: String,
    source_position: Option<Bytes>,
}

async fn process_bootstrap_source(
    query_id: String,
    mut input: QueryBootstrapInput,
    continuous_query: Arc<ContinuousQuery>,
    mode: QueryProcessingMode,
    output: Arc<OutputPublicationStage>,
    processing_lock: Arc<AsyncMutex<()>>,
) -> Result<BootstrapSourceHandoff> {
    let mut count = 0u64;
    while let Some(event) = input.receiver.recv().await {
        count = count.saturating_add(1);
        let _processing = processing_lock.lock().await;
        process_bootstrap_change(&continuous_query, &mode, &output, event.change)
            .await
            .with_context(|| {
                format!(
                    "query '{query_id}' failed bootstrap event {count} from source '{}'",
                    input.source_id
                )
            })?;
    }

    let source_position = match input.result_receiver {
        Some(receiver) => {
            let result = receiver
                .await
                .with_context(|| {
                    format!(
                        "source '{}': bootstrap result channel dropped without a result",
                        input.source_id
                    )
                })?
                .with_context(|| {
                    format!("source '{}' bootstrap provider failed", input.source_id)
                })?;
            if result.source_position.as_ref().is_some_and(|position| {
                position.len() > crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES
            }) {
                anyhow::bail!(
                    "source '{}' bootstrap position exceeds the {} byte limit",
                    input.source_id,
                    crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES
                );
            }
            result.source_position
        }
        None => None,
    };

    info!(
        "[BOOTSTRAP] Query '{query_id}' completed bootstrap from source '{}' ({count} events)",
        input.source_id
    );
    Ok(BootstrapSourceHandoff {
        source_id: input.source_id,
        source_position,
    })
}

async fn process_bootstrap_change(
    continuous_query: &ContinuousQuery,
    mode: &QueryProcessingMode,
    output: &OutputPublicationStage,
    change: SourceChange,
) -> Result<()> {
    let results = match mode {
        QueryProcessingMode::Atomic(resources) => continuous_query
            .process_source_change_with_result_hook(
                change,
                &resources.transaction,
                |results| async move {
                    output
                        .stage_bootstrap_live_results(&resources.live_results_writer, &results)
                        .await
                },
            )
            .await
            .context("atomic bootstrap envelope transaction failed")?,
        QueryProcessingMode::Legacy => {
            let results: Arc<[QueryPartEvaluationContext]> = continuous_query
                .process_source_change(change)
                .await
                .context("legacy bootstrap envelope evaluation failed")?
                .into();
            output
                .persist_bootstrap_live_results(&results)
                .await
                .context("legacy bootstrap live-result persistence failed")?;
            results
        }
    };

    output.apply_bootstrap_results(&results).await;
    Ok(())
}

/// Static, query-specific host for the current fixed live pipeline.
///
/// This intentionally has no generic pipeline traits. It gives each existing live
/// responsibility one owner while preserving the current orchestration contract.
pub(super) struct QueryCompositeHost {
    runtime: QueryHostRuntime,
    bootstrap: QueryBootstrapScope,
    live_input: LiveInputStage,
    future_processing: FutureProcessingStage,
    source_acknowledgement: SourceAcknowledgement,
    publication_recovery: AtomicPublicationRecovery,
}

impl QueryCompositeHost {
    pub(super) fn new(
        runtime: QueryHostRuntime,
        bootstrap: QueryBootstrapScope,
        mode: QueryProcessingMode,
        live: QueryLiveDependencies,
        output: QueryOutputDependencies,
    ) -> Self {
        let publication_recovery = output.publication_recovery.clone();
        let output = Arc::new(OutputPublicationStage::new(output));
        let continuous_query = live.continuous_query;

        Self {
            runtime,
            bootstrap,
            live_input: LiveInputStage {
                continuous_query: continuous_query.clone(),
                checkpoint_store: live.checkpoint_store,
                output: output.clone(),
                mode: mode.clone(),
            },
            future_processing: FutureProcessingStage {
                continuous_query,
                output,
                mode,
            },
            source_acknowledgement: SourceAcknowledgement::new(
                live.checkpoint_sequences,
                live.position_handles,
            ),
            publication_recovery,
        }
    }

    /// Spawn the host task and register its shutdown and join handles with `QueryBase`.
    pub(super) async fn start(self, base: &QueryBase) {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        base.set_shutdown_tx(shutdown_tx).await;

        let span = tracing::info_span!(
            "query_processor",
            instance_id = %self.runtime.instance_id,
            component_id = %self.runtime.query_id,
            component_type = "query"
        );
        let handle = tokio::spawn(self.run(shutdown_rx).instrument(span));
        base.set_task_handle(handle).await;
    }

    /// Complete the host-owned processor task through the existing QueryBase lifecycle.
    pub(super) async fn stop(base: &QueryBase) -> Result<()> {
        base.stop_common().await
    }

    /// Rebuild process-local output state from the authoritative durable bundle.
    ///
    /// This runs before source subscriptions are installed. Atomic bundles must
    /// reconcile successfully; persistent legacy bundles are reconciled when all
    /// three durable output resources are available.
    pub(super) async fn reconcile_output_state(
        mode: &QueryProcessingMode,
        dependencies: &QueryOutputDependencies,
    ) -> Result<()> {
        let (checkpoint_store, outbox_writer, live_results_writer) = match mode {
            QueryProcessingMode::Atomic(resources) => (
                resources.checkpoint_store.clone(),
                resources.outbox_writer.clone(),
                resources.live_results_writer.clone(),
            ),
            QueryProcessingMode::Legacy => {
                let (Some(checkpoint_store), Some(outbox_writer), Some(live_results_writer)) = (
                    dependencies.checkpoint_store.clone(),
                    dependencies.outbox_writer.clone(),
                    dependencies.live_results_writer.clone(),
                ) else {
                    anyhow::ensure!(
                        !dependencies.publication_recovery.is_required(),
                        "atomic publication recovery requires a complete durable output bundle"
                    );
                    return Ok(());
                };
                if !checkpoint_store.is_persistent() {
                    return Ok(());
                }
                (checkpoint_store, outbox_writer, live_results_writer)
            }
        };

        let query_id = dependencies.query_id.as_str();
        let previous_sequence = dependencies.output_state.read().await.as_of_sequence();
        let recovery_required = dependencies.publication_recovery.is_required();
        let pending_result = dependencies.publication_recovery.pending_result();
        let (result_sequence, outbox_latest, durable_outbox, durable_rows) = tokio::try_join!(
            checkpoint_store.read_result_sequence(query_id),
            outbox_writer.read_latest_sequence(query_id),
            outbox_writer.read_from(query_id, 0),
            live_results_writer.read_snapshot(query_id),
        )
        .context("failed to read the durable query output bundle")?;
        let durable_sequence = result_sequence.unwrap_or(0);
        anyhow::ensure!(
            durable_sequence < u64::MAX,
            "durable result sequence is exhausted at u64::MAX"
        );

        if durable_sequence == 0 {
            anyhow::ensure!(
                outbox_latest.is_none() && durable_outbox.is_empty(),
                "durable output is inconsistent: result sequence is 0 but the outbox is not empty"
            );
        } else {
            anyhow::ensure!(
                outbox_latest == Some(durable_sequence),
                "durable output is inconsistent: result sequence is {durable_sequence}, \
                 outbox latest is {outbox_latest:?}"
            );
            anyhow::ensure!(
                !durable_outbox.is_empty(),
                "durable output is inconsistent: result sequence is {durable_sequence}, \
                 but the outbox has no retained entry"
            );
        }

        let mut decoded_outbox = Vec::with_capacity(durable_outbox.len());
        let mut prior = None;
        for (stored_sequence, data) in durable_outbox {
            if let Some(prior) = prior {
                anyhow::ensure!(
                    stored_sequence == prior + 1,
                    "durable outbox gap between sequences {prior} and {stored_sequence}"
                );
            }
            let result: QueryResult = rmp_serde::from_slice(&data).with_context(|| {
                format!("failed to deserialize durable outbox sequence {stored_sequence}")
            })?;
            anyhow::ensure!(
                result.query_id == query_id,
                "durable outbox sequence {stored_sequence} belongs to query '{}', expected '{query_id}'",
                result.query_id
            );
            anyhow::ensure!(
                result.sequence == stored_sequence,
                "durable outbox key {stored_sequence} contains result sequence {}",
                result.sequence
            );
            let result = match pending_result
                .as_ref()
                .filter(|pending| pending.sequence == stored_sequence)
            {
                Some(pending) => {
                    anyhow::ensure!(
                        rmp_serde::to_vec(pending.as_ref())? == data,
                        "process-local fenced result sequence {stored_sequence} does not match durable outbox bytes"
                    );
                    pending.clone()
                }
                None => Arc::new(result),
            };
            prior = Some(stored_sequence);
            decoded_outbox.push(result);
        }
        anyhow::ensure!(
            prior == outbox_latest,
            "durable outbox latest metadata {outbox_latest:?} does not match retained entries ending at {prior:?}"
        );

        let mut results = im::HashMap::new();
        for (row_signature, data) in durable_rows {
            let value = rmp_serde::from_slice(&data).with_context(|| {
                format!("failed to deserialize durable live-result row {row_signature}")
            })?;
            anyhow::ensure!(
                results.insert(row_signature, value).is_none(),
                "durable live-result snapshot contains duplicate row {row_signature}"
            );
        }

        if let Some(latest) = decoded_outbox.last() {
            let mut latest_mutations = HashMap::new();
            for diff in &latest.results {
                match diff {
                    ResultDiff::Add {
                        data,
                        row_signature,
                    } => {
                        latest_mutations.insert(*row_signature, Some(data));
                    }
                    ResultDiff::Update {
                        after,
                        row_signature,
                        ..
                    }
                    | ResultDiff::Aggregation {
                        after,
                        row_signature,
                        ..
                    } => {
                        latest_mutations.insert(*row_signature, Some(after));
                    }
                    ResultDiff::Delete { row_signature, .. } => {
                        latest_mutations.insert(*row_signature, None);
                    }
                    ResultDiff::Noop => {}
                }
            }
            for (row_signature, expected) in latest_mutations {
                match expected {
                    Some(expected) => anyhow::ensure!(
                        results.get(&row_signature) == Some(expected),
                        "durable live-result row {row_signature} does not reflect outbox sequence {}",
                        latest.sequence
                    ),
                    None => anyhow::ensure!(
                        !results.contains_key(&row_signature),
                        "durable live-result row {row_signature} survived delete in outbox sequence {}",
                        latest.sequence
                    ),
                }
            }
        }

        anyhow::ensure!(
            previous_sequence <= durable_sequence,
            "process-local output sequence {previous_sequence} is ahead of durable sequence {durable_sequence}"
        );
        let recovered = if recovery_required {
            let pending_sequence = pending_result
                .as_ref()
                .map(|result| result.sequence)
                .context("publication recovery was required without a pending result")?;
            anyhow::ensure!(
                pending_sequence == durable_sequence,
                "fenced output sequence {pending_sequence} does not match durable sequence {durable_sequence}"
            );
            // The fence records the one commit that missed publication. Older
            // retained outbox entries were already delivered and must not be rebroadcast.
            let recovered: Vec<_> = decoded_outbox
                .iter()
                .filter(|result| result.sequence == pending_sequence)
                .cloned()
                .collect();
            anyhow::ensure!(
                recovered.len() == 1,
                "durable outbox does not contain fenced output sequence {pending_sequence}"
            );
            recovered
        } else {
            Vec::new()
        };

        {
            let mut state = dependencies.output_state.write().await;
            state.hydrate(results, durable_sequence, decoded_outbox);
            dependencies
                .output_metrics
                .record_live_results_count(state.results_len());
            let earliest = state.outbox_earliest_seq().unwrap_or(0);
            dependencies.output_metrics.update_outbox(
                state.outbox_len(),
                earliest,
                state.as_of_sequence(),
            );
        }

        dependencies.publication_recovery.reconcile_in_memory();
        for result in recovered {
            let dispatchers = dependencies.dispatchers.read().await;
            for dispatcher in dispatchers.iter() {
                if let Err(error) = dispatcher.dispatch_change(result.clone()).await {
                    debug!(
                        "Query '{query_id}' could not deliver recovered sequence {} to an existing subscriber: {error}",
                        result.sequence
                    );
                }
            }
        }
        Ok(())
    }

    async fn run(mut self, mut shutdown_rx: oneshot::Receiver<()>) {
        info!(
            "Query '{}' starting host-owned bootstrap",
            self.runtime.query_id
        );

        let bootstrap_result = tokio::select! {
            biased;

            _ = &mut shutdown_rx => {
                info!(
                    "Query '{}' received shutdown during bootstrap, exiting",
                    self.runtime.query_id
                );
                return;
            }

            result = self.bootstrap.run(
                &self.runtime.instance_id,
                &self.runtime.query_id,
                self.live_input.continuous_query.clone(),
                self.live_input.mode.clone(),
                self.live_input.output.clone(),
            ) => result,
        };

        let bootstrapped_sources = match bootstrap_result {
            Ok(sources) => sources,
            Err(error) => {
                let detail = format!("{error:#}");
                error!(
                    "[BOOTSTRAP] Query '{}' bootstrap failed: {detail}",
                    self.runtime.query_id
                );
                self.runtime.ingress_fence.close().await;
                self.runtime.future_queue_source.stop().await;
                self.runtime
                    .status_handle
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Bootstrap failed: {detail}")),
                    )
                    .await;
                return;
            }
        };
        for source_id in bootstrapped_sources {
            self.source_acknowledgement.advance(&source_id, Some(0));
        }

        let should_run = matches!(
            self.runtime.status_handle.get_status().await,
            ComponentStatus::Starting
        );
        if should_run {
            self.runtime
                .status_handle
                .set_status(
                    ComponentStatus::Running,
                    Some("Query started successfully".to_string()),
                )
                .await;
        } else {
            let current = self.runtime.status_handle.get_status().await;
            warn!(
                "Query '{}' bootstrap completed with status {current:?}; skipping live processing",
                self.runtime.query_id
            );
            self.runtime.ingress_fence.close().await;
            return;
        }

        if let Err(e) = self.runtime.future_queue_source.start().await {
            error!(
                "Query '{}' failed to start FutureQueueSource: {e}",
                self.runtime.query_id
            );
            self.runtime.ingress_fence.close().await;
            self.runtime.future_queue_source.stop().await;
            self.runtime
                .status_handle
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Future queue start failed: {e}")),
                )
                .await;
            return;
        }

        info!(
            "Query '{}' starting priority queue event processor",
            self.runtime.query_id
        );

        loop {
            let current_status = self.runtime.status_handle.get_status().await;
            if !matches!(current_status, ComponentStatus::Running) {
                info!(
                    "Query '{}' status changed to non-running ({current_status:?}), exiting processing loop",
                    self.runtime.query_id
                );
                break;
            }

            tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    info!(
                        "Query '{}' received shutdown signal, exiting processing loop",
                        self.runtime.query_id
                    );
                    break;
                }

                arc_event = self.runtime.priority_queue.dequeue() => {
                    if let Err(e) = self.process_event(arc_event).await {
                        let detail = format!("{e:#}");
                        self.runtime.ingress_fence.close().await;
                        let status_detail = if self.publication_recovery.is_required() {
                            format!(
                                "Atomic output committed before in-memory publication; \
                                 ingress is fenced and will be reconciled from durable \
                                 output before restart: {detail}"
                            )
                        } else {
                            format!(
                                "Atomic query processing failed before publication; \
                                 ingress is fenced and a clean stop/start can replay it: {detail}"
                            )
                        };
                        error!(
                            "Query '{}' {status_detail}",
                            self.runtime.query_id
                        );
                        self.runtime
                            .status_handle
                            .set_status(
                                ComponentStatus::Error,
                                Some(status_detail),
                            )
                            .await;
                        break;
                    }
                }
            }
        }

        self.runtime.ingress_fence.close().await;
        self.runtime.future_queue_source.stop().await;
        info!("Query '{}' processing task exited", self.runtime.query_id);
    }

    async fn process_event(&mut self, arc_event: Arc<SourceEventWrapper>) -> Result<()> {
        let Some(input) = LiveInput::from_wrapper(arc_event, &self.runtime.query_id) else {
            return Ok(());
        };

        debug!(
            "Query '{}' processing event from source '{}'",
            self.runtime.query_id, input.source_id
        );

        if self
            .source_acknowledgement
            .should_skip(input.source_id.as_ref(), input.sequence)
        {
            debug!(
                "Query '{}' skipping duplicate event from '{}' (seq={seq}, checkpoint={cp})",
                self.runtime.query_id,
                input.source_id,
                seq = input.sequence.unwrap_or(0),
                cp = self
                    .source_acknowledgement
                    .checkpoint_for(input.source_id.as_ref())
                    .unwrap_or(0)
            );
            return Ok(());
        }

        match input.event {
            SourceEvent::Control(SourceControl::FuturesDue) => {
                self.future_processing.drain_due().await?;
            }
            SourceEvent::Change(source_change) => {
                self.live_input
                    .process(
                        LiveChangeContext {
                            source_id: input.source_id,
                            source_change,
                            profiling: input.profiling,
                            sequence: input.sequence,
                            source_position: input.source_position,
                        },
                        &mut self.source_acknowledgement,
                    )
                    .await?;
            }
            SourceEvent::Control(_) => {
                debug!(
                    "Query '{}' ignoring control event from source '{}'",
                    self.runtime.query_id, input.source_id
                );
            }
        }

        Ok(())
    }
}

struct LiveInput {
    source_id: Arc<str>,
    event: SourceEvent,
    profiling: Option<ProfilingMetadata>,
    sequence: Option<u64>,
    source_position: Option<Bytes>,
}

impl LiveInput {
    fn from_wrapper(arc_event: Arc<SourceEventWrapper>, query_id: &str) -> Option<Self> {
        if matches!(&arc_event.event, SourceEvent::Change(_)) {
            // Move sole-owned channel events through the envelope. Shared broadcast
            // events retain the existing single-clone fallback.
            let envelope_result = match SourceEventWrapper::try_unwrap_arc(arc_event) {
                Ok(parts) => crate::change::source_event_parts_to_envelope(
                    parts,
                    crate::change::SystemMetadataExtensions::default(),
                ),
                Err(shared) => crate::change::source_event_to_envelope(
                    shared.as_ref(),
                    crate::change::SystemMetadataExtensions::default(),
                ),
            };
            let envelope = match envelope_result {
                Ok(envelope) => envelope,
                Err(e) => {
                    error!(
                        "Query '{query_id}' failed to adapt source event to a change envelope: {e}"
                    );
                    return None;
                }
            };
            let (source_id, profiling, sequence, source_position) = {
                let system = envelope.system();
                let Some(source_id) = system.source_id_arc().cloned() else {
                    error!(
                        "Query '{query_id}' received a graph change envelope without a source ID"
                    );
                    return None;
                };
                (
                    source_id,
                    system.profiling().cloned(),
                    system.sequence(),
                    system.source_position().cloned(),
                )
            };
            let source_change = match crate::change::source_change_from_envelope_owned(envelope) {
                Ok(change) => change,
                Err(e) => {
                    error!("Query '{query_id}' failed to read graph change envelope: {e}");
                    return None;
                }
            };

            return Some(Self {
                source_id,
                event: SourceEvent::Change(source_change),
                profiling,
                sequence,
                source_position,
            });
        }

        // Control events stay on the legacy path and retain priority-queue ordering.
        let parts = match SourceEventWrapper::try_unwrap_arc(arc_event) {
            Ok(parts) => parts,
            Err(arc) => crate::channels::events::SourceEventParts {
                source_id: arc.source_id.clone(),
                event: arc.event.clone(),
                timestamp: arc.timestamp,
                profiling: arc.profiling.clone(),
                sequence: arc.sequence,
                source_position: arc.source_position.clone(),
            },
        };

        Some(Self {
            source_id: Arc::<str>::from(parts.source_id),
            event: parts.event,
            profiling: parts.profiling,
            sequence: parts.sequence,
            source_position: parts.source_position,
        })
    }
}

struct LiveChangeContext {
    source_id: Arc<str>,
    source_change: SourceChange,
    profiling: Option<ProfilingMetadata>,
    sequence: Option<u64>,
    source_position: Option<Bytes>,
}

struct LiveInputStage {
    continuous_query: Arc<ContinuousQuery>,
    checkpoint_store: Arc<dyn CheckpointStore>,
    output: Arc<OutputPublicationStage>,
    mode: QueryProcessingMode,
}

impl LiveInputStage {
    async fn process(
        &self,
        input: LiveChangeContext,
        acknowledgement: &mut SourceAcknowledgement,
    ) -> Result<()> {
        match &self.mode {
            QueryProcessingMode::Atomic(resources) => {
                self.process_atomic(input, acknowledgement, resources).await
            }
            QueryProcessingMode::Legacy => {
                self.process_legacy(input, acknowledgement).await;
                Ok(())
            }
        }
    }

    async fn process_legacy(
        &self,
        input: LiveChangeContext,
        acknowledgement: &mut SourceAcknowledgement,
    ) {
        let mut profiling = match input.profiling {
            Some(profiling) => profiling,
            None => ProfilingMetadata::new(),
        };
        profiling.query_receive_ns = Some(crate::profiling::timestamp_ns());
        profiling.query_core_call_ns = Some(crate::profiling::timestamp_ns());

        // Legacy mode intentionally retains the A1 ordering. Atomic mode stages
        // checkpoint and output together in its result-aware hook.
        let checkpoint_store = self.checkpoint_store.clone();
        let checkpoint_source_id = input.source_id.clone();
        let checkpoint_position = input.source_position.clone();
        let sequence = input.sequence;
        let hook = move || async move {
            if let Some(sequence) = sequence {
                let position = match &checkpoint_position {
                    Some(position)
                        if position.len()
                            <= crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES =>
                    {
                        Some(position)
                    }
                    _ => None,
                };
                checkpoint_store
                    .stage_checkpoint(checkpoint_source_id.as_ref(), sequence, position)
                    .await?;
            }
            Ok(())
        };

        match self
            .continuous_query
            .process_source_change_with_hook(input.source_change, hook)
            .await
        {
            Ok(results) => {
                profiling.query_core_return_ns = Some(crate::profiling::timestamp_ns());

                acknowledgement.advance(input.source_id.as_ref(), input.sequence);

                if !results.is_empty() {
                    profiling.query_send_ns = Some(crate::profiling::timestamp_ns());
                    if let Err(e) = self
                        .output
                        .publish(&results, input.source_id.as_ref(), profiling)
                        .await
                    {
                        error!(
                            "Query '{}' failed to adapt query results: {e}",
                            self.output.query_id
                        );
                    }
                }
            }
            Err(e) => {
                error!(
                    "Query '{}' failed to process source change: {e}",
                    self.output.query_id
                );
            }
        }
    }

    async fn process_atomic(
        &self,
        input: LiveChangeContext,
        acknowledgement: &mut SourceAcknowledgement,
        resources: &AtomicQueryResources,
    ) -> Result<()> {
        let mut profiling = match input.profiling {
            Some(profiling) => profiling,
            None => ProfilingMetadata::new(),
        };
        profiling.query_receive_ns = Some(crate::profiling::timestamp_ns());
        profiling.query_core_call_ns = Some(crate::profiling::timestamp_ns());

        let prepared_slot: Arc<Mutex<Option<PreparedQueryOutput>>> = Arc::new(Mutex::new(None));
        let hook_slot = prepared_slot.clone();
        let output = self.output.clone();
        let staging_resources = resources.clone();
        let checkpoint_source_id = input.source_id.clone();
        let output_source_id = input.source_id.clone();
        let checkpoint_position = input.source_position.clone();
        let sequence = input.sequence;

        let transaction_result = self
            .continuous_query
            .process_source_change_with_result_hook(
                input.source_change,
                &resources.transaction,
                move |results| async move {
                    profiling.query_core_return_ns = Some(crate::profiling::timestamp_ns());

                    if let Some(sequence) = sequence {
                        staging_resources
                            .checkpoint_store
                            .stage_checkpoint(
                                checkpoint_source_id.as_ref(),
                                sequence,
                                valid_checkpoint_position(checkpoint_position.as_ref()),
                            )
                            .await?;
                    }

                    if has_query_output(&results) {
                        // In atomic mode this timestamp marks durable output staging,
                        // not post-commit dispatch. One materialized QueryResult is
                        // persisted and later dispatched unchanged.
                        profiling.query_send_ns = Some(crate::profiling::timestamp_ns());
                        let prepared = output
                            .prepare_atomic(&results, output_source_id.as_ref(), profiling)
                            .await
                            .map_err(index_error)?;
                        output.stage_atomic(&staging_resources, &prepared).await?;
                        let recovery_result = prepared.result.clone();
                        {
                            let mut slot = hook_slot.lock().map_err(|_| {
                                IndexError::other(std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    "prepared query output slot was poisoned",
                                ))
                            })?;
                            *slot = Some(prepared);
                        }
                        output.observe_output_staged().await?;
                        output.arm_publication_recovery(recovery_result);
                    } else {
                        output.observe_output_staged().await?;
                    }
                    Ok(())
                },
            )
            .await;
        let committed_results = match transaction_result {
            Ok(results) => results,
            Err(error) => {
                // A returned error means SessionGuard completed rollback. An abort
                // during commit never reaches here and leaves the fence armed.
                self.output.clear_publication_recovery();
                return Err(error).context("atomic source transaction failed before commit");
            }
        };

        // Keep this synchronous with the completed commit: cancellation must
        // never observe durable output before the recovery fence is armed.
        let publication_guard = self
            .output
            .committed_publication_guard(has_query_output(&committed_results));
        self.output
            .observe_commit_before_publish()
            .await
            .context("atomic source transaction committed before publication was interrupted")?;

        let prepared = prepared_slot
            .lock()
            .map_err(|_| anyhow::anyhow!("prepared query output slot was poisoned"))?
            .take();
        anyhow::ensure!(
            has_query_output(&committed_results) == prepared.is_some(),
            "committed source output did not match the prepared output slot"
        );

        let committed_result = match prepared {
            Some(prepared) => Some(self.output.apply_committed(prepared).await.context(
                "durable source output committed but in-memory publication invariant failed",
            )?),
            None => None,
        };
        // `apply_committed` is the only await between arming and clearing.
        publication_guard.complete();

        acknowledgement.advance(input.source_id.as_ref(), input.sequence);
        self.output.observe_source_acknowledged();
        if let Some(result) = committed_result {
            self.output.dispatch(result).await;
        }

        Ok(())
    }
}

fn valid_checkpoint_position(position: Option<&Bytes>) -> Option<&Bytes> {
    position.filter(|position| {
        position.len() <= crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES
    })
}

fn has_query_output(results: &[QueryPartEvaluationContext]) -> bool {
    results
        .iter()
        .any(|result| !matches!(result, QueryPartEvaluationContext::Noop))
}

fn index_error(error: anyhow::Error) -> IndexError {
    IndexError::other(std::io::Error::new(
        std::io::ErrorKind::Other,
        error.to_string(),
    ))
}

struct SourceAcknowledgement {
    dedup: SequenceDedup,
    position_handles: HashMap<String, Arc<AtomicU64>>,
}

impl SourceAcknowledgement {
    fn new(
        checkpoint_sequences: HashMap<String, u64>,
        position_handles: HashMap<String, Arc<AtomicU64>>,
    ) -> Self {
        Self {
            dedup: SequenceDedup::new(checkpoint_sequences),
            position_handles,
        }
    }

    fn should_skip(&self, source_id: &str, sequence: Option<u64>) -> bool {
        self.dedup.should_skip(source_id, sequence)
    }

    fn checkpoint_for(&self, source_id: &str) -> Option<u64> {
        self.dedup.checkpoint_for(source_id)
    }

    fn advance(&mut self, source_id: &str, sequence: Option<u64>) {
        let Some(sequence) = sequence else {
            return;
        };

        self.dedup.advance(source_id, sequence);
        if let Some(handle) = self.position_handles.get(source_id) {
            handle.store(sequence, Ordering::Release);
        }
    }
}

struct FutureProcessingStage {
    continuous_query: Arc<ContinuousQuery>,
    output: Arc<OutputPublicationStage>,
    mode: QueryProcessingMode,
}

impl FutureProcessingStage {
    async fn drain_due(&self) -> Result<()> {
        match &self.mode {
            QueryProcessingMode::Atomic(resources) => self.drain_due_atomic(resources).await,
            QueryProcessingMode::Legacy => {
                self.drain_due_legacy().await;
                Ok(())
            }
        }
    }

    async fn drain_due_legacy(&self) {
        loop {
            match self.continuous_query.process_due_futures().await {
                Ok(Some(due_result)) => {
                    if !due_result.results.is_empty() {
                        let profiling = ProfilingMetadata::new();
                        if let Err(e) = self
                            .output
                            .publish(&due_result.results, &due_result.source_id, profiling)
                            .await
                        {
                            error!(
                                "Query '{}' failed to adapt due-future results: {e}",
                                self.output.query_id
                            );
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!(
                        "Query '{}' failed to process due futures: {e}",
                        self.output.query_id
                    );
                    break;
                }
            }
        }
    }

    async fn drain_due_atomic(&self, resources: &AtomicQueryResources) -> Result<()> {
        loop {
            let next_due_time = self
                .continuous_query
                .future_queue()
                .peek_due_time()
                .await
                .context("failed to inspect the next atomic due future")?;
            let Some(next_due_time) = next_due_time else {
                break;
            };
            if next_due_time > current_time_millis() {
                break;
            }

            let prepared_slot: Arc<Mutex<Option<PreparedQueryOutput>>> = Arc::new(Mutex::new(None));
            let hook_slot = prepared_slot.clone();
            let output = self.output.clone();
            let staging_resources = resources.clone();
            let mut profiling = ProfilingMetadata::new();
            profiling.query_core_call_ns = Some(crate::profiling::timestamp_ns());

            let transaction_result = self
                .continuous_query
                .process_due_futures_with_result_hook(
                    &resources.transaction,
                    move |due_result| async move {
                        profiling.query_core_return_ns = Some(crate::profiling::timestamp_ns());

                        if has_query_output(&due_result.results) {
                            profiling.query_send_ns = Some(crate::profiling::timestamp_ns());
                            let prepared = output
                                .prepare_atomic(
                                    &due_result.results,
                                    &due_result.source_id,
                                    profiling,
                                )
                                .await
                                .map_err(index_error)?;
                            output.stage_atomic(&staging_resources, &prepared).await?;
                            let recovery_result = prepared.result.clone();
                            {
                                let mut slot = hook_slot.lock().map_err(|_| {
                                    IndexError::other(std::io::Error::new(
                                        std::io::ErrorKind::Other,
                                        "prepared due-future output slot was poisoned",
                                    ))
                                })?;
                                *slot = Some(prepared);
                            }
                            output.observe_output_staged().await?;
                            output.arm_publication_recovery(recovery_result);
                        } else {
                            output.observe_output_staged().await?;
                        }
                        Ok(())
                    },
                )
                .await;
            let due_result = match transaction_result {
                Ok(result) => result,
                Err(error) => {
                    // A returned error means SessionGuard completed rollback. An
                    // abort during commit leaves this recovery fence armed.
                    self.output.clear_publication_recovery();
                    return Err(error)
                        .context("atomic due-future transaction failed before commit");
                }
            };

            let Some(due_result) = due_result else {
                break;
            };

            // Keep this synchronous with the completed commit: cancellation must
            // never observe durable output before the recovery fence is armed.
            let publication_guard = self
                .output
                .committed_publication_guard(has_query_output(&due_result.results));
            self.output.observe_commit_before_publish().await.context(
                "atomic due-future transaction committed before publication was interrupted",
            )?;

            let prepared = prepared_slot
                .lock()
                .map_err(|_| anyhow::anyhow!("prepared due-future output slot was poisoned"))?
                .take();
            anyhow::ensure!(
                has_query_output(&due_result.results) == prepared.is_some(),
                "committed due-future output did not match the prepared output slot"
            );

            if let Some(prepared) = prepared {
                let result = self
                        .output
                        .apply_committed(prepared)
                        .await
                        .context(
                            "durable due-future output committed but in-memory publication invariant failed",
                        )?;
                // `apply_committed` is the only await between arming and clearing.
                publication_guard.complete();
                self.output.dispatch(result).await;
            } else {
                publication_guard.complete();
            }
        }

        Ok(())
    }
}

fn current_time_millis() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

#[derive(Clone)]
struct OutputPublicationStage {
    query_id: String,
    output_state: Arc<RwLock<QueryOutputState>>,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
    outbox_writer: Option<Arc<dyn OutboxWriter>>,
    live_results_writer: Option<Arc<dyn LiveResultsWriter>>,
    checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    outbox_capacity: usize,
    output_metrics: Arc<QueryOutputMetrics>,
    observer: Option<Arc<dyn QueryProcessingObserver>>,
    publication_recovery: AtomicPublicationRecovery,
}

impl OutputPublicationStage {
    fn new(dependencies: QueryOutputDependencies) -> Self {
        Self {
            query_id: dependencies.query_id,
            output_state: dependencies.output_state,
            dispatchers: dependencies.dispatchers,
            outbox_writer: dependencies.outbox_writer,
            live_results_writer: dependencies.live_results_writer,
            checkpoint_store: dependencies.checkpoint_store,
            outbox_capacity: dependencies.outbox_capacity,
            output_metrics: dependencies.output_metrics,
            observer: dependencies.observer,
            publication_recovery: dependencies.publication_recovery,
        }
    }

    async fn dispatch_bootstrap_control(&self, signal: &str, source_count: Option<usize>) {
        let mut metadata = HashMap::new();
        metadata.insert(
            "control_signal".to_string(),
            serde_json::Value::String(signal.to_string()),
        );
        if let Some(source_count) = source_count {
            metadata.insert("source_count".to_string(), source_count.into());
        }
        let result = Arc::new(QueryResult::new(
            self.query_id.clone(),
            0,
            chrono::Utc::now(),
            Vec::new(),
            metadata,
        ));

        let dispatchers = self.dispatchers.read().await;
        for dispatcher in dispatchers.iter() {
            if let Err(error) = dispatcher.dispatch_change(result.clone()).await {
                debug!(
                    "Query '{}' failed to dispatch {signal}: {error}",
                    self.query_id
                );
            }
        }
    }

    async fn reset_bootstrap_state(&self) {
        self.output_state.write().await.reset();
        self.output_metrics.record_live_results_count(0);
        self.output_metrics.update_outbox(0, 0, 0);
    }

    async fn stage_bootstrap_live_results(
        &self,
        writer: &Arc<dyn LiveResultsWriter>,
        results: &[QueryPartEvaluationContext],
    ) -> std::result::Result<(), IndexError> {
        let diffs = result_diffs_from_evaluation(results);
        let serialized = materialize_live_result_data(&diffs).map_err(index_error)?;
        apply_live_result_data(writer.as_ref(), &self.query_id, &serialized).await
    }

    async fn persist_bootstrap_live_results(
        &self,
        results: &[QueryPartEvaluationContext],
    ) -> Result<()> {
        let Some(writer) = &self.live_results_writer else {
            return Ok(());
        };
        let diffs = result_diffs_from_evaluation(results);
        let serialized = materialize_live_result_data(&diffs)?;
        apply_live_result_data(writer.as_ref(), &self.query_id, &serialized)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn apply_bootstrap_results(&self, results: &[QueryPartEvaluationContext]) {
        let diffs = result_diffs_from_evaluation(results);
        if diffs.is_empty() {
            return;
        }

        let mut state = self.output_state.write().await;
        state.apply_diffs(&diffs);
        self.output_metrics
            .record_live_results_count(state.results_len());
    }

    async fn publish(
        &self,
        results: &[QueryPartEvaluationContext],
        source_id: &str,
        profiling: ProfilingMetadata,
    ) -> Result<()> {
        dispatch_query_results(
            results,
            source_id,
            &self.query_id,
            &self.output_state,
            &self.dispatchers,
            &self.outbox_writer,
            &self.live_results_writer,
            &self.checkpoint_store,
            self.outbox_capacity,
            profiling,
            &self.output_metrics,
        )
        .await
    }

    async fn prepare_atomic(
        &self,
        results: &[QueryPartEvaluationContext],
        source_id: &str,
        profiling: ProfilingMetadata,
    ) -> Result<PreparedQueryOutput> {
        // A7 will hydrate this state from durable output during startup. A6
        // intentionally preserves the existing in-memory sequence origin.
        let expected_sequence = self.output_state.read().await.next_sequence();
        let mut prepared = PreparedQueryOutput::from_evaluation(
            results,
            source_id,
            &self.query_id,
            expected_sequence,
            profiling,
            Instant::now(),
        )?
        .ok_or_else(|| anyhow::anyhow!("non-Noop query evaluation produced no change envelope"))?;
        prepared.materialize_atomic()?;
        Ok(prepared)
    }

    async fn stage_atomic(
        &self,
        resources: &AtomicQueryResources,
        prepared: &PreparedQueryOutput,
    ) -> std::result::Result<(), IndexError> {
        let outbox_data = prepared
            .outbox_data
            .as_deref()
            .ok_or(IndexError::CorruptedData)?;
        let live_result_data = prepared
            .live_result_data
            .as_ref()
            .ok_or(IndexError::CorruptedData)?;

        resources
            .outbox_writer
            .append(&self.query_id, prepared.result.sequence, outbox_data)
            .await?;
        resources
            .outbox_writer
            .trim_to_capacity(&self.query_id, self.outbox_capacity)
            .await?;

        let row_mutations: Vec<RowMutation<'_>> = live_result_data
            .iter()
            .map(|(signature, data)| RowMutation {
                row_signature: *signature,
                data: data.as_deref(),
            })
            .collect();
        if !row_mutations.is_empty() {
            resources
                .live_results_writer
                .apply_mutations(&self.query_id, &row_mutations)
                .await?;
        }

        resources
            .checkpoint_store
            .write_result_sequence(&self.query_id, prepared.result.sequence)
            .await
    }

    async fn observe_output_staged(&self) -> std::result::Result<(), IndexError> {
        match &self.observer {
            Some(observer) => observer.after_output_staged().await,
            None => Ok(()),
        }
    }

    async fn observe_commit_before_publish(&self) -> Result<()> {
        match &self.observer {
            Some(observer) => observer.after_commit_before_publish().await,
            None => Ok(()),
        }
    }

    fn committed_publication_guard(&self, has_output: bool) -> CommittedPublicationGuard {
        CommittedPublicationGuard::new(self.publication_recovery.clone(), has_output)
    }

    fn arm_publication_recovery(&self, result: Arc<QueryResult>) {
        self.publication_recovery.require(result);
    }

    fn clear_publication_recovery(&self) {
        self.publication_recovery.reconcile_in_memory();
    }

    fn observe_source_acknowledged(&self) {
        if let Some(observer) = &self.observer {
            observer.after_source_acknowledged();
        }
    }

    async fn apply_committed(&self, prepared: PreparedQueryOutput) -> Result<Arc<QueryResult>> {
        let PreparedQueryOutput {
            result, started_at, ..
        } = prepared;
        let mut state = self.output_state.write().await;
        let result = state.apply_committed_arc(result)?;

        let duration_ns = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.output_metrics
            .record_transaction_duration_ns(duration_ns);
        self.output_metrics.record_seq_advance();
        self.output_metrics
            .record_live_results_count(state.results_len());
        let earliest_seq = state.outbox_earliest_seq().unwrap_or(0);
        self.output_metrics
            .update_outbox(state.outbox_len(), earliest_seq, state.as_of_sequence());

        Ok(result)
    }

    async fn dispatch(&self, result: Arc<QueryResult>) {
        debug!(
            "Query '{}' sending {} results to reactions (seq={})",
            self.query_id,
            result.results.len(),
            result.sequence
        );

        let dispatchers = self.dispatchers.read().await;
        for dispatcher in dispatchers.iter() {
            if let Err(e) = dispatcher.dispatch_change(result.clone()).await {
                debug!(
                    "Failed to dispatch result for query '{}': {e}",
                    self.query_id
                );
            }
        }
    }
}

fn result_diffs_from_evaluation(results: &[QueryPartEvaluationContext]) -> Vec<ResultDiff> {
    results
        .iter()
        .map(|context| match context {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            } => ResultDiff::Add {
                data: query_variables_to_json(after),
                row_signature: *row_signature,
            },
            QueryPartEvaluationContext::Removing {
                before,
                row_signature,
            } => ResultDiff::Delete {
                data: query_variables_to_json(before),
                row_signature: *row_signature,
            },
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } => {
                let after = query_variables_to_json(after);
                ResultDiff::Update {
                    data: after.clone(),
                    before: query_variables_to_json(before),
                    after,
                    grouping_keys: None,
                    row_signature: *row_signature,
                }
            }
            QueryPartEvaluationContext::Aggregation {
                before,
                after,
                row_signature,
                ..
            } => ResultDiff::Aggregation {
                before: before.as_ref().map(query_variables_to_json),
                after: query_variables_to_json(after),
                row_signature: *row_signature,
            },
            QueryPartEvaluationContext::Noop => ResultDiff::Noop,
        })
        .collect()
}

pub(super) fn query_variables_to_json(
    variables: &drasi_core::evaluation::context::QueryVariables,
) -> serde_json::Value {
    let values = variables
        .iter()
        .map(|(key, value)| (key.to_string(), variable_value_to_json(value)))
        .collect();
    serde_json::Value::Object(values)
}

pub(super) fn variable_value_to_json(
    value: &drasi_core::evaluation::variable_value::VariableValue,
) -> serde_json::Value {
    use drasi_core::evaluation::variable_value::VariableValue;

    match value {
        VariableValue::Null => serde_json::Value::Null,
        VariableValue::Bool(value) => serde_json::Value::Bool(*value),
        VariableValue::Float(value) => {
            if value.is_f64() {
                let value = value.to_string();
                value
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or_else(|| serde_json::Value::String(value))
            } else {
                serde_json::Value::String(value.to_string())
            }
        }
        VariableValue::Integer(value) => value
            .as_i64()
            .map(serde_json::Number::from)
            .or_else(|| value.as_u64().map(serde_json::Number::from))
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(value.to_string())),
        VariableValue::String(value) => serde_json::Value::String(value.clone()),
        VariableValue::List(values) => {
            serde_json::Value::Array(values.iter().map(variable_value_to_json).collect())
        }
        VariableValue::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), variable_value_to_json(value)))
                .collect(),
        ),
        VariableValue::Date(value) => serde_json::Value::String(value.to_string()),
        VariableValue::LocalTime(value) => serde_json::Value::String(value.to_string()),
        VariableValue::ZonedTime(value) => serde_json::Value::String(value.to_string()),
        VariableValue::LocalDateTime(value) => serde_json::Value::String(value.to_string()),
        VariableValue::ZonedDateTime(value) => {
            serde_json::Value::String(value.datetime().to_rfc3339())
        }
        VariableValue::Duration(value) => serde_json::Value::String(value.to_string()),
        _ => serde_json::Value::String(format!("{value:?}")),
    }
}

fn materialize_live_result_data(diffs: &[ResultDiff]) -> Result<Vec<(u64, Option<Vec<u8>>)>> {
    let mut serialized = Vec::with_capacity(diffs.len());
    for diff in diffs {
        match diff {
            ResultDiff::Add {
                data,
                row_signature,
            } => serialized.push((
                *row_signature,
                Some(
                    rmp_serde::to_vec(data)
                        .with_context(|| format!("failed to serialize Add row {row_signature}"))?,
                ),
            )),
            ResultDiff::Update {
                after,
                row_signature,
                ..
            }
            | ResultDiff::Aggregation {
                after,
                row_signature,
                ..
            } => serialized.push((
                *row_signature,
                Some(
                    rmp_serde::to_vec(after)
                        .with_context(|| format!("failed to serialize row {row_signature}"))?,
                ),
            )),
            ResultDiff::Delete { row_signature, .. } => {
                serialized.push((*row_signature, None));
            }
            ResultDiff::Noop => {}
        }
    }
    Ok(serialized)
}

async fn apply_live_result_data(
    writer: &dyn LiveResultsWriter,
    query_id: &str,
    serialized: &[(u64, Option<Vec<u8>>)],
) -> std::result::Result<(), IndexError> {
    let mutations: Vec<_> = serialized
        .iter()
        .map(|(row_signature, data)| RowMutation {
            row_signature: *row_signature,
            data: data.as_deref(),
        })
        .collect();
    if mutations.is_empty() {
        return Ok(());
    }
    writer.apply_mutations(query_id, &mutations).await
}

struct PreparedQueryOutput {
    result: Arc<QueryResult>,
    started_at: Instant,
    outbox_data: Option<Vec<u8>>,
    live_result_data: Option<Vec<(u64, Option<Vec<u8>>)>>,
}

impl PreparedQueryOutput {
    fn from_evaluation(
        results: &[QueryPartEvaluationContext],
        source_id: &str,
        query_id: &str,
        sequence: u64,
        profiling: ProfilingMetadata,
        started_at: Instant,
    ) -> Result<Option<Self>> {
        let result_count = results
            .iter()
            .filter(|result| !matches!(result, QueryPartEvaluationContext::Noop))
            .count();
        if result_count == 0 {
            return Ok(None);
        }

        let mut metadata = HashMap::new();
        metadata.insert(
            "source_id".to_string(),
            serde_json::Value::String(source_id.to_string()),
        );
        metadata.insert(
            "processed_by".to_string(),
            serde_json::Value::String("drasi-core".to_string()),
        );
        metadata.insert(
            "result_count".to_string(),
            serde_json::Value::Number(result_count.into()),
        );
        let envelope = crate::change::query_evaluation_to_envelope(
            results,
            crate::change::QueryEnvelopeMetadata::new(
                Arc::<str>::from(query_id),
                Some(Arc::<str>::from(source_id)),
                sequence,
                chrono::Utc::now(),
                metadata,
                Some(profiling),
            ),
        )?
        .ok_or_else(|| anyhow::anyhow!("non-Noop query evaluation produced no change envelope"))?;

        Ok(Some(Self {
            result: Arc::new(crate::change::query_result_from_envelope(&envelope)?),
            started_at,
            outbox_data: None,
            live_result_data: None,
        }))
    }

    fn materialize_atomic(&mut self) -> Result<()> {
        self.outbox_data = Some(
            rmp_serde::to_vec(self.result.as_ref())
                .context("failed to serialize prepared query result for the atomic outbox")?,
        );
        self.live_result_data = Some(materialize_live_result_data(&self.result.results)?);
        Ok(())
    }
}

/// Prepare, persist, and dispatch a legacy `QueryResult`.
///
/// Canonicalization and serialization stay outside the output-state write lock.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_query_results(
    results: &[QueryPartEvaluationContext],
    source_id: &str,
    query_id: &str,
    output_state: &RwLock<QueryOutputState>,
    dispatchers: &RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>,
    outbox_writer: &Option<Arc<dyn OutboxWriter>>,
    live_results_writer: &Option<Arc<dyn LiveResultsWriter>>,
    checkpoint_store: &Option<Arc<dyn CheckpointStore>>,
    outbox_capacity: usize,
    profiling: ProfilingMetadata,
    output_metrics: &Arc<QueryOutputMetrics>,
) -> Result<()> {
    let tx_start = Instant::now();

    let arc_result = loop {
        let expected_sequence = output_state.read().await.next_sequence();
        let Some(prepared) = PreparedQueryOutput::from_evaluation(
            results,
            source_id,
            query_id,
            expected_sequence,
            profiling.clone(),
            tx_start,
        )?
        else {
            return Ok(());
        };
        let query_result = prepared.result;

        let mut state = output_state.write().await;
        let result = match state.try_apply_prepared_arc(query_result) {
            Some(result) => result,
            None => continue,
        };

        let duration_ns = u64::try_from(tx_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        output_metrics.record_transaction_duration_ns(duration_ns);
        output_metrics.record_seq_advance();
        output_metrics.record_live_results_count(state.results_len());
        let earliest_seq = state.outbox_earliest_seq().unwrap_or(0);
        output_metrics.update_outbox(state.outbox_len(), earliest_seq, state.as_of_sequence());

        break result;
    };

    // Keep the characterized ordering: core/checkpoint commit precedes these
    // best-effort output writes, and delivery still occurs after persistence attempts.
    let mut outbox_ok = true;
    if let Some(writer) = outbox_writer {
        match rmp_serde::to_vec(arc_result.as_ref()) {
            Ok(data) => {
                if let Err(e) = writer.append(query_id, arc_result.sequence, &data).await {
                    warn!(
                        "Query '{query_id}' failed to persist result seq={} to outbox: {e}",
                        arc_result.sequence
                    );
                    outbox_ok = false;
                }
            }
            Err(e) => {
                warn!(
                    "Query '{query_id}' failed to serialize result seq={} for outbox: {e}",
                    arc_result.sequence
                );
                outbox_ok = false;
            }
        }

        if outbox_ok {
            if let Err(e) = writer.trim_to_capacity(query_id, outbox_capacity).await {
                warn!("Query '{query_id}' failed to trim persistent outbox: {e}");
            }
        }
    }

    let mut live_results_ok = true;
    if let Some(writer) = live_results_writer {
        use drasi_core::interface::RowMutation;

        let serialized_data: Vec<(u64, Option<Vec<u8>>)> = arc_result
            .results
            .iter()
            .filter_map(|diff| match diff {
                ResultDiff::Add {
                    data,
                    row_signature,
                } => match rmp_serde::to_vec(data) {
                    Ok(serialized) => Some((*row_signature, Some(serialized))),
                    Err(e) => {
                        warn!(
                            "Query '{query_id}' failed to serialize Add row (sig={row_signature}) for live results: {e}"
                        );
                        None
                    }
                },
                ResultDiff::Update {
                    after,
                    row_signature,
                    ..
                } => match rmp_serde::to_vec(after) {
                    Ok(serialized) => Some((*row_signature, Some(serialized))),
                    Err(e) => {
                        warn!(
                            "Query '{query_id}' failed to serialize Update row (sig={row_signature}) for live results: {e}"
                        );
                        None
                    }
                },
                ResultDiff::Aggregation {
                    after,
                    row_signature,
                    ..
                } => match rmp_serde::to_vec(after) {
                    Ok(serialized) => Some((*row_signature, Some(serialized))),
                    Err(e) => {
                        warn!(
                            "Query '{query_id}' failed to serialize Aggregation row (sig={row_signature}) for live results: {e}"
                        );
                        None
                    }
                },
                ResultDiff::Delete { row_signature, .. } => Some((*row_signature, None)),
                ResultDiff::Noop => None,
            })
            .collect();

        let row_mutations: Vec<RowMutation<'_>> = serialized_data
            .iter()
            .map(|(signature, data)| RowMutation {
                row_signature: *signature,
                data: data.as_deref(),
            })
            .collect();

        if !row_mutations.is_empty() {
            if let Err(e) = writer.apply_mutations(query_id, &row_mutations).await {
                warn!(
                    "Query '{query_id}' failed to persist live results for seq={}: {e}",
                    arc_result.sequence
                );
                live_results_ok = false;
            }
        }
    }

    if outbox_ok && live_results_ok {
        if let Some(store) = checkpoint_store {
            if let Err(e) = store
                .write_result_sequence(query_id, arc_result.sequence)
                .await
            {
                warn!(
                    "Query '{query_id}' failed to write result sequence {}: {e}",
                    arc_result.sequence
                );
            }
        }
    }

    debug!(
        "Query '{query_id}' sending {} results to reactions (seq={})",
        arc_result.results.len(),
        arc_result.sequence
    );

    let dispatchers = dispatchers.read().await;
    for dispatcher in dispatchers.iter() {
        if let Err(e) = dispatcher.dispatch_change(arc_result.clone()).await {
            debug!("Failed to dispatch result for query '{query_id}': {e}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::atomic::{AtomicBool, AtomicUsize},
        time::Duration,
    };

    use async_trait::async_trait;
    use drasi_core::{
        evaluation::{
            functions::{Function, FunctionRegistry, ScalarFunction},
            variable_value::VariableValue,
            ExpressionEvaluationContext, FunctionError, FunctionEvaluationError,
        },
        in_memory_index::{
            in_memory_checkpoint_store::InMemoryCheckpointStore,
            in_memory_future_queue::InMemoryFutureQueue,
        },
        interface::{FutureElementRef, FutureQueue, IndexError, PushType, SourceCheckpoint},
        models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp,
        },
        query::QueryBuilder,
    };
    use drasi_functions_cypher::CypherFunctionSet;
    use drasi_query_ast::api::QueryConfiguration;
    use drasi_query_cypher::CypherParser;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        bootstrap::BootstrapResult,
        channels::{
            BootstrapEvent, ChangeReceiver, ChannelChangeDispatcher, ControlOperation,
            SourceControl,
        },
        config::{QueryConfig, QueryLanguage},
    };

    const QUERY_ID: &str = "host-query";
    const SOURCE_ID: &str = "host-source";

    struct TestQueryConfig;

    impl QueryConfiguration for TestQueryConfig {
        fn get_aggregating_function_names(&self) -> HashSet<String> {
            ["count", "sum", "min", "max", "avg", "collect", "stdev", "stdevp"]
                .into_iter()
                .map(str::to_string)
                .collect()
        }
    }

    struct FailingScalar;

    #[async_trait]
    impl ScalarFunction for FailingScalar {
        async fn call(
            &self,
            _context: &ExpressionEvaluationContext,
            expression: &drasi_query_ast::ast::FunctionExpression,
            _args: Vec<VariableValue>,
        ) -> std::result::Result<VariableValue, FunctionError> {
            Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            })
        }
    }

    struct HostFixture {
        base: QueryBase,
        priority_queue: PriorityQueue,
        output_state: Arc<RwLock<QueryOutputState>>,
        output_rx: Box<dyn ChangeReceiver<QueryResult>>,
        future_queue_source: Arc<FutureQueueSource>,
    }

    async fn build_query(
        query: &str,
        future_queue: Option<Arc<dyn FutureQueue>>,
    ) -> Arc<ContinuousQuery> {
        let parser = Arc::new(CypherParser::new(Arc::new(TestQueryConfig)));
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let mut builder =
            QueryBuilder::new(query, parser).with_function_registry(function_registry);
        if let Some(future_queue) = future_queue {
            builder = builder.with_future_queue(future_queue);
        }
        Arc::new(builder.build().await)
    }

    async fn build_failing_query() -> Arc<ContinuousQuery> {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("fail", Function::Scalar(Arc::new(FailingScalar)));
        let parser = Arc::new(CypherParser::new(functions.clone()));
        Arc::new(
            QueryBuilder::new("MATCH (n:Person) RETURN fail() AS value", parser)
                .with_function_registry(functions)
                .build()
                .await,
        )
    }

    async fn build_host(
        continuous_query: Arc<ContinuousQuery>,
        checkpoint_store: Arc<dyn CheckpointStore>,
        checkpoint_sequences: HashMap<String, u64>,
        position_handles: HashMap<String, Arc<AtomicU64>>,
    ) -> (QueryCompositeHost, HostFixture) {
        build_host_with_bootstrap(
            continuous_query,
            checkpoint_store,
            checkpoint_sequences,
            position_handles,
            Vec::new(),
        )
        .await
    }

    async fn build_host_with_bootstrap(
        continuous_query: Arc<ContinuousQuery>,
        checkpoint_store: Arc<dyn CheckpointStore>,
        checkpoint_sequences: HashMap<String, u64>,
        position_handles: HashMap<String, Arc<AtomicU64>>,
        bootstrap_inputs: Vec<QueryBootstrapInput>,
    ) -> (QueryCompositeHost, HostFixture) {
        let base = QueryBase::new(QueryConfig {
            id: QUERY_ID.to_string(),
            query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![],
            auto_start: false,
            joins: None,
            enable_bootstrap: false,
            bootstrap_buffer_size: 10,
            priority_queue_capacity: Some(16),
            dispatch_buffer_capacity: Some(16),
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 16,
            bootstrap_timeout_secs: 1,
        })
        .unwrap();
        base.set_status(ComponentStatus::Starting, None).await;

        let dispatcher = ChannelChangeDispatcher::<QueryResult>::new(16);
        let output_rx = dispatcher.create_receiver().await.unwrap();
        let dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>> =
            Arc::new(RwLock::new(vec![Box::new(dispatcher)]));
        let output_state = Arc::new(RwLock::new(QueryOutputState::new(16)));
        let priority_queue = PriorityQueue::new(16);
        let future_queue_source = Arc::new(FutureQueueSource::new(
            continuous_query.future_queue(),
            QUERY_ID.to_string(),
        ));

        let host = QueryCompositeHost::new(
            QueryHostRuntime::new(
                "host-test".to_string(),
                QUERY_ID.to_string(),
                priority_queue.clone(),
                base.status_handle(),
                future_queue_source.clone(),
                QueryIngressFence::new(priority_queue.clone()),
            ),
            QueryBootstrapScope::new(bootstrap_inputs, checkpoint_store.clone(), None),
            QueryProcessingMode::legacy(),
            QueryLiveDependencies::new(
                continuous_query,
                checkpoint_store.clone(),
                checkpoint_sequences,
                position_handles,
            ),
            QueryOutputDependencies::new(
                QUERY_ID.to_string(),
                output_state.clone(),
                dispatchers,
                None,
                None,
                Some(checkpoint_store),
                16,
                Arc::new(QueryOutputMetrics::new()),
            ),
        );

        (
            host,
            HostFixture {
                base,
                priority_queue,
                output_state,
                output_rx,
                future_queue_source,
            },
        )
    }

    fn person_insert(node_id: &str, name: &str, effective_from: u64) -> SourceChange {
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(SOURCE_ID, node_id),
                    labels: vec!["Person".into()].into(),
                    effective_from,
                },
                properties: ElementPropertyMap::from(json!({ "name": name })),
            },
        }
    }

    fn sequenced_event(
        change: SourceChange,
        sequence: u64,
        source_position: Bytes,
    ) -> Arc<SourceEventWrapper> {
        let timestamp =
            chrono::DateTime::from_timestamp_millis(change.get_realtime() as i64).unwrap();
        let mut event = SourceEventWrapper::with_sequence(
            SOURCE_ID.to_string(),
            SourceEvent::Change(change),
            timestamp,
            sequence,
            Some(ProfilingMetadata {
                source_ns: Some(101),
                source_receive_ns: Some(202),
                source_send_ns: Some(303),
                ..Default::default()
            }),
        );
        event.set_source_position(source_position);
        Arc::new(event)
    }

    async fn wait_for_status(base: &QueryBase, expected: ComponentStatus) {
        let mut status_rx = base.status_handle().subscribe_status();
        tokio::time::timeout(
            Duration::from_secs(2),
            status_rx.wait_for(|status| *status == expected),
        )
        .await
        .expect("status transition timed out")
        .expect("status channel closed");
    }

    async fn receive_result(
        receiver: &mut Box<dyn ChangeReceiver<QueryResult>>,
    ) -> Arc<QueryResult> {
        tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("query result timed out")
            .expect("query result channel closed")
    }

    async fn stop_host(fixture: &HostFixture) {
        fixture
            .base
            .set_status(ComponentStatus::Stopping, None)
            .await;
        QueryCompositeHost::stop(&fixture.base).await.unwrap();
        assert_eq!(fixture.base.get_status().await, ComponentStatus::Stopped);
        assert!(fixture.base.task_handle.read().await.is_none());
        assert!(fixture.base.shutdown_tx.read().await.is_none());
    }

    #[tokio::test]
    async fn host_processes_live_envelopes_and_terminates_through_query_base() {
        let query = build_query("MATCH (n:Person) RETURN n.name AS name", None).await;
        let checkpoint_store = Arc::new(InMemoryCheckpointStore::new());
        let position_handle = Arc::new(AtomicU64::new(u64::MAX));
        let mut position_handles = HashMap::new();
        position_handles.insert(SOURCE_ID.to_string(), position_handle.clone());
        let (host, mut fixture) = build_host(
            query,
            checkpoint_store.clone(),
            HashMap::new(),
            position_handles,
        )
        .await;

        let source_position = Bytes::from_static(b"host-position-1");
        assert!(
            fixture
                .priority_queue
                .enqueue(sequenced_event(
                    person_insert("person-1", "Ada", 1_700_000_000_000),
                    11,
                    source_position.clone(),
                ))
                .await
        );

        host.start(&fixture.base).await;
        wait_for_status(&fixture.base, ComponentStatus::Running).await;
        let result = receive_result(&mut fixture.output_rx).await;

        assert_eq!(result.query_id, QUERY_ID);
        assert_eq!(result.sequence, 1);
        assert_eq!(result.metadata["source_id"], SOURCE_ID);
        assert_eq!(result.profiling.as_ref().unwrap().source_ns, Some(101));
        assert_eq!(
            checkpoint_store.read_checkpoint(SOURCE_ID).await.unwrap(),
            Some(SourceCheckpoint::new(11, Some(source_position)))
        );
        assert_eq!(position_handle.load(Ordering::Acquire), 11);
        assert_eq!(fixture.priority_queue.metrics().await.total_dequeued, 1);

        stop_host(&fixture).await;
    }

    #[tokio::test]
    async fn host_bootstrap_handoff_keeps_live_input_buffered_until_completion() {
        let query = build_query("MATCH (n:Person) RETURN n.name AS name", None).await;
        let checkpoint_store = Arc::new(InMemoryCheckpointStore::new());
        let position_handle = Arc::new(AtomicU64::new(u64::MAX));
        let (bootstrap_tx, bootstrap_rx) = mpsc::channel(4);
        let (result_tx, result_rx) = oneshot::channel();
        let (host, mut fixture) = build_host_with_bootstrap(
            query,
            checkpoint_store.clone(),
            HashMap::new(),
            HashMap::from([(SOURCE_ID.to_string(), position_handle.clone())]),
            vec![QueryBootstrapInput::new(SOURCE_ID.to_string(), bootstrap_rx, Some(result_rx))],
        )
        .await;

        assert!(
            fixture
                .priority_queue
                .enqueue(sequenced_event(
                    person_insert("person-live", "Live", 2_000),
                    1,
                    Bytes::from_static(b"live-position"),
                ))
                .await
        );
        host.start(&fixture.base).await;

        let started = receive_result(&mut fixture.output_rx).await;
        assert_eq!(started.sequence, 0);
        assert_eq!(started.metadata["control_signal"], "bootstrapStarted");
        bootstrap_tx
            .send(BootstrapEvent {
                source_id: SOURCE_ID.to_string(),
                change: person_insert("person-bootstrap", "Bootstrap", 1_000),
                timestamp: chrono::Utc::now(),
                sequence: 0,
            })
            .await
            .expect("send bootstrap event");
        tokio::task::yield_now().await;
        assert_eq!(fixture.base.get_status().await, ComponentStatus::Starting);
        assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
        assert_eq!(fixture.priority_queue.metrics().await.total_dequeued, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), fixture.output_rx.recv())
                .await
                .is_err(),
            "live output must remain gated while bootstrap is open"
        );

        result_tx
            .send(Ok(BootstrapResult {
                event_count: 1,
                source_position: Some(Bytes::from_static(b"bootstrap-boundary")),
            }))
            .expect("send bootstrap result");
        drop(bootstrap_tx);

        let completed = receive_result(&mut fixture.output_rx).await;
        assert_eq!(completed.sequence, 0);
        assert_eq!(completed.metadata["control_signal"], "bootstrapCompleted");
        wait_for_status(&fixture.base, ComponentStatus::Running).await;
        let live = receive_result(&mut fixture.output_rx).await;
        assert_eq!(live.sequence, 1);
        assert_eq!(fixture.output_state.read().await.results_len(), 2);
        assert_eq!(
            checkpoint_store
                .read_checkpoint(SOURCE_ID)
                .await
                .expect("read live checkpoint"),
            Some(SourceCheckpoint::new(
                1,
                Some(Bytes::from_static(b"live-position"))
            ))
        );
        assert_eq!(position_handle.load(Ordering::Acquire), 1);

        stop_host(&fixture).await;
    }

    #[tokio::test]
    async fn bootstrap_evaluation_failure_fences_without_completion_and_can_rebootstrap() {
        let checkpoint_store = Arc::new(InMemoryCheckpointStore::new());
        let (bootstrap_tx, bootstrap_rx) = mpsc::channel(2);
        let (result_tx, result_rx) = oneshot::channel();
        let (host, mut failed_fixture) = build_host_with_bootstrap(
            build_failing_query().await,
            checkpoint_store.clone(),
            HashMap::new(),
            HashMap::new(),
            vec![QueryBootstrapInput::new(SOURCE_ID.to_string(), bootstrap_rx, Some(result_rx))],
        )
        .await;
        host.start(&failed_fixture.base).await;
        let started = receive_result(&mut failed_fixture.output_rx).await;
        assert_eq!(started.metadata["control_signal"], "bootstrapStarted");

        bootstrap_tx
            .send(BootstrapEvent {
                source_id: SOURCE_ID.to_string(),
                change: person_insert("person-fails", "Failure", 1_000),
                timestamp: chrono::Utc::now(),
                sequence: 0,
            })
            .await
            .expect("send failing bootstrap event");
        drop(bootstrap_tx);
        let _ = result_tx.send(Ok(BootstrapResult::default()));
        wait_for_status(&failed_fixture.base, ComponentStatus::Error).await;
        assert_eq!(failed_fixture.output_state.read().await.results_len(), 0);
        assert!(
            checkpoint_store
                .read_checkpoint(SOURCE_ID)
                .await
                .expect("read failed-bootstrap checkpoint")
                .is_none(),
            "a failed bootstrap must not publish its handoff checkpoint"
        );
        if let Ok(Ok(result)) =
            tokio::time::timeout(Duration::from_millis(50), failed_fixture.output_rx.recv()).await
        {
            assert_ne!(
                result.metadata["control_signal"], "bootstrapCompleted",
                "a failed bootstrap must not emit bootstrapCompleted"
            );
        }
        stop_host(&failed_fixture).await;

        let (retry_tx, retry_rx) = mpsc::channel(2);
        let (retry_result_tx, retry_result_rx) = oneshot::channel();
        let (retry_host, mut retry_fixture) = build_host_with_bootstrap(
            build_query("MATCH (n:Person) RETURN n.name AS name", None).await,
            checkpoint_store.clone(),
            HashMap::new(),
            HashMap::new(),
            vec![QueryBootstrapInput::new(SOURCE_ID.to_string(), retry_rx, Some(retry_result_rx))],
        )
        .await;
        retry_host.start(&retry_fixture.base).await;
        let _ = receive_result(&mut retry_fixture.output_rx).await;
        retry_tx
            .send(BootstrapEvent {
                source_id: SOURCE_ID.to_string(),
                change: person_insert("person-retry", "Recovered", 2_000),
                timestamp: chrono::Utc::now(),
                sequence: 0,
            })
            .await
            .expect("send retry bootstrap event");
        retry_result_tx
            .send(Ok(BootstrapResult::default()))
            .expect("send retry bootstrap result");
        drop(retry_tx);
        let completed = receive_result(&mut retry_fixture.output_rx).await;
        assert_eq!(completed.metadata["control_signal"], "bootstrapCompleted");
        wait_for_status(&retry_fixture.base, ComponentStatus::Running).await;
        assert_eq!(retry_fixture.output_state.read().await.results_len(), 1);
        stop_host(&retry_fixture).await;
    }

    #[tokio::test]
    async fn host_ignores_other_control_and_drains_futures_due() {
        let future_queue = Arc::new(InMemoryFutureQueue::new());
        let query = build_query(
            "
            MATCH (o:Order)
            WHERE o.status = 'ready'
              AND drasi.trueFor(sum(o.value) > 1, duration({ seconds: 5 }))
            RETURN sum(o.value) AS value
            ",
            Some(future_queue.clone()),
        )
        .await;
        let initial = query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("future-source", "order-1"),
                        labels: vec!["Order".into()].into(),
                        effective_from: 1_577_836_800_000,
                    },
                    properties: ElementPropertyMap::from(json!({
                        "status": "ready",
                        "value": 2,
                    })),
                },
            })
            .await
            .unwrap();
        assert!(initial.is_empty());
        assert!(future_queue.peek_due_time().await.unwrap().is_some());

        let checkpoint_store = Arc::new(InMemoryCheckpointStore::new());
        let (host, mut fixture) =
            build_host(query, checkpoint_store, HashMap::new(), HashMap::new()).await;
        host.start(&fixture.base).await;
        wait_for_status(&fixture.base, ComponentStatus::Running).await;

        let now = chrono::Utc::now();
        assert!(
            fixture
                .priority_queue
                .enqueue(Arc::new(SourceEventWrapper::new(
                    SOURCE_ID.to_string(),
                    SourceEvent::Control(SourceControl::Subscription {
                        query_id: QUERY_ID.to_string(),
                        query_node_id: "node-1".to_string(),
                        node_labels: vec![],
                        rel_labels: vec![],
                        operation: ControlOperation::Insert,
                    }),
                    now,
                )))
                .await
        );
        assert!(
            fixture
                .priority_queue
                .enqueue(Arc::new(SourceEventWrapper::new(
                    "__future_queue__".to_string(),
                    SourceEvent::Control(SourceControl::FuturesDue),
                    now + chrono::Duration::milliseconds(1),
                )))
                .await
        );

        let result = receive_result(&mut fixture.output_rx).await;
        assert_eq!(result.sequence, 1);
        assert_eq!(result.metadata["source_id"], "future-source");
        assert_eq!(fixture.priority_queue.metrics().await.total_dequeued, 2);
        assert!(future_queue.peek_due_time().await.unwrap().is_none());

        stop_host(&fixture).await;
    }

    struct FailOnceCheckpointStore {
        inner: InMemoryCheckpointStore,
        failed: AtomicBool,
        attempt_tx: mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl CheckpointStore for FailOnceCheckpointStore {
        fn is_persistent(&self) -> bool {
            false
        }

        async fn stage_checkpoint(
            &self,
            source_id: &str,
            sequence: u64,
            source_position: Option<&Bytes>,
        ) -> Result<(), IndexError> {
            let _ = self.attempt_tx.send(());
            if !self.failed.swap(true, Ordering::AcqRel) {
                return Err(IndexError::other(std::io::Error::other(
                    "injected checkpoint failure",
                )));
            }
            self.inner
                .stage_checkpoint(source_id, sequence, source_position)
                .await
        }

        async fn read_checkpoint(
            &self,
            source_id: &str,
        ) -> Result<Option<SourceCheckpoint>, IndexError> {
            self.inner.read_checkpoint(source_id).await
        }

        async fn read_all_checkpoints(
            &self,
        ) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
            self.inner.read_all_checkpoints().await
        }

        async fn clear_checkpoints(&self) -> Result<(), IndexError> {
            self.inner.clear_checkpoints().await
        }

        async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
            self.inner.write_config_hash(hash).await
        }

        async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
            self.inner.read_config_hash().await
        }

        async fn write_result_sequence(
            &self,
            query_id: &str,
            sequence: u64,
        ) -> Result<(), IndexError> {
            self.inner.write_result_sequence(query_id, sequence).await
        }

        async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
            self.inner.read_result_sequence(query_id).await
        }
    }

    #[tokio::test]
    async fn live_processing_error_keeps_host_running_and_does_not_acknowledge() {
        let query = build_query("MATCH (n:Person) RETURN n.name AS name", None).await;
        let (attempt_tx, mut attempt_rx) = mpsc::unbounded_channel();
        let checkpoint_store = Arc::new(FailOnceCheckpointStore {
            inner: InMemoryCheckpointStore::new(),
            failed: AtomicBool::new(false),
            attempt_tx,
        });
        let position_handle = Arc::new(AtomicU64::new(u64::MAX));
        let mut position_handles = HashMap::new();
        position_handles.insert(SOURCE_ID.to_string(), position_handle.clone());
        let (host, mut fixture) = build_host(
            query,
            checkpoint_store.clone(),
            HashMap::new(),
            position_handles,
        )
        .await;
        host.start(&fixture.base).await;
        wait_for_status(&fixture.base, ComponentStatus::Running).await;

        let source_position = Bytes::from_static(b"retry-position");
        let change = person_insert("person-retry", "Grace", 1_700_000_000_100);
        assert!(
            fixture
                .priority_queue
                .enqueue(sequenced_event(change.clone(), 17, source_position.clone(),))
                .await
        );
        tokio::time::timeout(Duration::from_secs(2), attempt_rx.recv())
            .await
            .expect("checkpoint attempt timed out")
            .expect("checkpoint attempt channel closed");
        assert_eq!(position_handle.load(Ordering::Acquire), u64::MAX);
        assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
        assert_eq!(fixture.base.get_status().await, ComponentStatus::Running);

        assert!(
            fixture
                .priority_queue
                .enqueue(sequenced_event(change, 17, source_position.clone()))
                .await
        );
        let result = receive_result(&mut fixture.output_rx).await;
        assert_eq!(result.sequence, 1);
        assert_eq!(
            checkpoint_store.read_checkpoint(SOURCE_ID).await.unwrap(),
            Some(SourceCheckpoint::new(17, Some(source_position)))
        );
        assert_eq!(position_handle.load(Ordering::Acquire), 17);

        stop_host(&fixture).await;
    }

    struct FailOncePopFutureQueue {
        inner: InMemoryFutureQueue,
        pop_calls: AtomicUsize,
        pop_tx: mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl FutureQueue for FailOncePopFutureQueue {
        async fn push(
            &self,
            push_type: PushType,
            position_in_query: usize,
            group_signature: u64,
            element_ref: &ElementReference,
            original_time: ElementTimestamp,
            due_time: ElementTimestamp,
        ) -> Result<bool, IndexError> {
            self.inner
                .push(
                    push_type,
                    position_in_query,
                    group_signature,
                    element_ref,
                    original_time,
                    due_time,
                )
                .await
        }

        async fn remove(
            &self,
            position_in_query: usize,
            group_signature: u64,
        ) -> Result<(), IndexError> {
            self.inner.remove(position_in_query, group_signature).await
        }

        async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
            let attempt = self.pop_calls.fetch_add(1, Ordering::AcqRel);
            let _ = self.pop_tx.send(());
            if attempt == 0 {
                return Err(IndexError::other(std::io::Error::other(
                    "injected future pop failure",
                )));
            }
            self.inner.pop().await
        }

        async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
            self.inner.peek_due_time().await
        }

        async fn clear(&self) -> Result<(), IndexError> {
            self.inner.clear().await
        }
    }

    #[tokio::test]
    async fn due_future_error_breaks_drain_and_keeps_host_running() {
        let (pop_tx, mut pop_rx) = mpsc::unbounded_channel();
        let future_queue = Arc::new(FailOncePopFutureQueue {
            inner: InMemoryFutureQueue::new(),
            pop_calls: AtomicUsize::new(0),
            pop_tx,
        });
        let query = build_query(
            "MATCH (n:Person) RETURN n.name AS name",
            Some(future_queue.clone()),
        )
        .await;
        let checkpoint_store = Arc::new(InMemoryCheckpointStore::new());
        let (host, mut fixture) =
            build_host(query, checkpoint_store, HashMap::new(), HashMap::new()).await;
        host.start(&fixture.base).await;
        wait_for_status(&fixture.base, ComponentStatus::Running).await;

        assert!(
            fixture
                .priority_queue
                .enqueue(Arc::new(SourceEventWrapper::new(
                    "__future_queue__".to_string(),
                    SourceEvent::Control(SourceControl::FuturesDue),
                    chrono::Utc::now(),
                )))
                .await
        );
        tokio::time::timeout(Duration::from_secs(2), pop_rx.recv())
            .await
            .expect("future pop attempt timed out")
            .expect("future pop attempt channel closed");

        assert!(
            fixture
                .priority_queue
                .enqueue(Arc::new(SourceEventWrapper::new(
                    SOURCE_ID.to_string(),
                    SourceEvent::Change(person_insert(
                        "person-after-future-error",
                        "Katherine",
                        1_700_000_000_200,
                    )),
                    chrono::DateTime::from_timestamp_millis(1_700_000_000_200).unwrap(),
                )))
                .await
        );
        let result = receive_result(&mut fixture.output_rx).await;
        assert_eq!(result.sequence, 1);
        assert_eq!(future_queue.pop_calls.load(Ordering::Acquire), 1);
        assert_eq!(fixture.base.get_status().await, ComponentStatus::Running);

        stop_host(&fixture).await;
    }

    #[tokio::test]
    async fn future_queue_start_failure_sets_error_and_task_terminates() {
        let query = build_query("MATCH (n:Person) RETURN n.name AS name", None).await;
        let checkpoint_store = Arc::new(InMemoryCheckpointStore::new());
        let (host, fixture) =
            build_host(query, checkpoint_store, HashMap::new(), HashMap::new()).await;

        fixture.future_queue_source.start().await.unwrap();
        host.start(&fixture.base).await;
        wait_for_status(&fixture.base, ComponentStatus::Error).await;

        fixture.future_queue_source.stop().await;
        QueryCompositeHost::stop(&fixture.base).await.unwrap();
        assert_eq!(fixture.base.get_status().await, ComponentStatus::Stopped);
        assert!(fixture.base.task_handle.read().await.is_none());
        assert!(fixture.base.shutdown_tx.read().await.is_none());
    }
}

#[cfg(test)]
mod atomic_tests;
