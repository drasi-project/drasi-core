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
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{oneshot, Mutex, RwLock};

// Import drasi-core components
use drasi_core::{
    evaluation::functions::FunctionRegistry,
    in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore,
    interface::{CheckpointStore, LiveResultsWriter, OutboxWriter},
    middleware::MiddlewareTypeRegistry,
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_functions_gql::GQLFunctionSet;
use drasi_query_ast::api::{QueryConfiguration, QueryParser};
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;

use crate::channels::*;
use crate::component_graph::{ComponentGraph, ComponentKind, ComponentUpdateSender};
use crate::config::SourceSubscriptionSettings;
use crate::config::{QueryConfig, QueryLanguage, QueryRuntime};
use crate::managers::{
    log_component_error, log_component_start, log_component_stop, ComponentLogKey,
    ComponentLogRegistry,
};
use crate::metrics::QueryOutputMetrics;
use crate::queries::label_extractor::{LabelExtractor, QueryLabels};
use crate::queries::output_state::{
    FetchError, OutboxGap, OutboxResponse, QueryOutputState, SnapshotResponse,
};
#[cfg(test)]
use crate::queries::query_composite_host::dispatch_query_results;
use crate::queries::query_composite_host::{
    read_legacy_pending_state, read_output_reset_high_water, AtomicPublicationRecovery,
    LegacyPendingState, QueryBootstrapInput, QueryBootstrapRecoveryState, QueryBootstrapScope,
    QueryCompositeHost, QueryHostRuntime, QueryIngressFence, QueryLiveDependencies,
    QueryOutputDependencies, QueryProcessingMode, QueryProcessingObserver,
    LEGACY_OUTPUT_PENDING_MARKER_V1, QUERY_BOOTSTRAP_MARKER_V1, QUERY_OUTPUT_RESET_MARKER_V1,
};
use crate::queries::PriorityQueue;
use crate::queries::QueryBase;
use crate::sources::FutureQueueSource;
use crate::sources::Source;
use crate::sources::SourceManager;
use tracing::Instrument;

/// Default query configuration
struct DefaultQueryConfig;

impl QueryConfiguration for DefaultQueryConfig {
    fn get_aggregating_function_names(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("count".into());
        set.insert("sum".into());
        set.insert("min".into());
        set.insert("max".into());
        set.insert("avg".into());
        set.insert("collect".into());
        set.insert("stdev".into());
        set.insert("stdevp".into());
        set
    }
}

#[cfg(test)]
use crate::queries::query_composite_host::{
    query_variables_to_json as convert_query_variables_to_json,
    variable_value_to_json as convert_variable_value_to_json,
};

#[cfg(test)]
mod tests {
    use super::convert_variable_value_to_json;
    use chrono::{Duration as ChronoDuration, FixedOffset, NaiveDate, NaiveTime, TimeZone};
    use drasi_core::evaluation::variable_value::{
        duration::Duration as VarDuration, zoned_datetime::ZonedDateTime as VarZonedDateTime,
        zoned_time::ZonedTime as VarZonedTime, VariableValue,
    };

    #[test]
    fn temporal_values_serialize_as_plain_strings() {
        let date = NaiveDate::from_ymd_opt(2024, 6, 15).expect("valid date");
        let local_time = NaiveTime::from_hms_micro_opt(10, 30, 45, 123_456).expect("valid time");
        let offset = FixedOffset::east_opt(3600).expect("valid fixed offset");
        let zoned_time = VarZonedTime::new(local_time, offset);
        let local_datetime = date
            .and_hms_micro_opt(10, 30, 45, 123_456)
            .expect("valid local datetime");
        let zoned_datetime = VarZonedDateTime::new(
            offset
                .with_ymd_and_hms(2024, 6, 15, 10, 30, 45)
                .single()
                .expect("valid zoned datetime"),
            Some("Europe/Berlin".to_string()),
        );
        let duration = VarDuration::new(ChronoDuration::seconds(90), 0, 0);

        let date_json = convert_variable_value_to_json(&VariableValue::Date(date));
        assert_eq!(date_json, serde_json::Value::String(date.to_string()));

        let local_time_json = convert_variable_value_to_json(&VariableValue::LocalTime(local_time));
        assert_eq!(
            local_time_json,
            serde_json::Value::String(local_time.to_string())
        );

        let zoned_time_json = convert_variable_value_to_json(&VariableValue::ZonedTime(zoned_time));
        assert_eq!(
            zoned_time_json,
            serde_json::Value::String(zoned_time.to_string())
        );

        let local_datetime_json =
            convert_variable_value_to_json(&VariableValue::LocalDateTime(local_datetime));
        assert_eq!(
            local_datetime_json,
            serde_json::Value::String(local_datetime.to_string())
        );

        let zoned_datetime_json =
            convert_variable_value_to_json(&VariableValue::ZonedDateTime(zoned_datetime.clone()));
        assert_eq!(
            zoned_datetime_json,
            serde_json::Value::String(zoned_datetime.datetime().to_rfc3339())
        );

        let duration_json =
            convert_variable_value_to_json(&VariableValue::Duration(duration.clone()));
        assert_eq!(
            duration_json,
            serde_json::Value::String(duration.to_string())
        );
    }
}

#[async_trait]
pub trait Query: Send + Sync {
    /// Start the query - subscribes to sources and begins processing events
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &QueryConfig;
    fn as_any(&self) -> &dyn std::any::Any;

    /// Return the number of active subscription forwarder tasks (diagnostic/testing).
    async fn subscription_count(&self) -> usize {
        0
    }

    /// Subscribe to query results for reactions
    /// Returns a broadcast receiver for Arc-wrapped QueryResults
    async fn subscribe(&self, reaction_id: String) -> Result<QuerySubscriptionResponse>;

    /// Fetch a snapshot of the live result set.
    ///
    /// Returns the current results (as an `im::HashMap` clone — O(1) via structural sharing)
    /// and the `as_of_sequence` reflecting the latest emission.
    ///
    /// Blocks until bootstrap completes. Returns `FetchError::TimedOut` if bootstrap
    /// does not complete within 5 minutes, or `FetchError::NotRunning` if the query
    /// terminates in a non-Running state.
    async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError>;

    /// Fetch outbox entries after the given sequence number.
    ///
    /// Returns `Ok(OutboxResponse)` if the requested position is still in the ring buffer,
    /// or `Err(FetchError::OutboxGap)` if it has been evicted.
    ///
    /// Blocks until bootstrap completes, with the same timeout/error semantics as
    /// `fetch_snapshot`.
    async fn fetch_outbox(&self, after_sequence: u64) -> Result<OutboxResponse, FetchError>;

    /// Get the query's output metrics (outbox health, sequence rate, snapshot tracking).
    ///
    /// Returns `None` for query implementations that don't support metrics.
    fn output_metrics(&self) -> Option<Arc<QueryOutputMetrics>> {
        None
    }

    /// Release the persistent index-backend handles this query retains, **without**
    /// deleting any on-disk data.
    ///
    /// Persistent backends (e.g. RocksDB, which holds a process-exclusive lock on its
    /// data directory) keep that resource pinned until every clone of their shared
    /// handle is dropped. A query retains some of those handles for its whole lifetime,
    /// so they are only freed when the whole `DrasiLib` is dropped. This method drops
    /// those in-memory handles so the backend can release its lock during a permanent
    /// shutdown, while leaving persisted state intact for a future reopen.
    ///
    /// Default: no-op — volatile/in-memory queries hold no such handles.
    async fn release_persistent_handles(&self) {}
}

struct StartupCancellationRegistration {
    slot: Arc<StdMutex<Option<oneshot::Sender<()>>>>,
}

impl StartupCancellationRegistration {
    fn install(slot: Arc<StdMutex<Option<oneshot::Sender<()>>>>) -> (Self, oneshot::Receiver<()>) {
        let (sender, receiver) = oneshot::channel();
        let mut current = slot
            .lock()
            .expect("query startup cancellation lock poisoned");
        debug_assert!(
            current.is_none(),
            "query startup cancellation was not cleared"
        );
        *current = Some(sender);
        drop(current);
        (Self { slot }, receiver)
    }
}

impl Drop for StartupCancellationRegistration {
    fn drop(&mut self) {
        self.slot
            .lock()
            .expect("query startup cancellation lock poisoned")
            .take();
    }
}

struct StopRequestReset(Arc<AtomicBool>);

impl Drop for StopRequestReset {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct DrasiQuery {
    // DrasiLib instance ID for log routing isolation
    instance_id: String,
    // Use QueryBase for common functionality
    base: QueryBase,
    output_state: Arc<RwLock<QueryOutputState>>,
    // Pre-computed config hash for bootstrap APIs
    config_hash: u64,
    // Priority queue for ordered event processing
    priority_queue: PriorityQueue,
    // Reference to SourceManager for direct subscription
    source_manager: Arc<SourceManager>,
    // Own every source/future forwarder that can enqueue into the query.
    ingress_fence: QueryIngressFence,
    // Serialize start/stop so task handles cannot be replaced during cleanup.
    lifecycle_lock: Arc<Mutex<()>>,
    // Lets stop cancel a source subscription before waiting for lifecycle serialization.
    startup_cancel: Arc<StdMutex<Option<oneshot::Sender<()>>>>,
    // Level-triggered stop intent closes the gap before startup registers its sender.
    stop_requested: Arc<AtomicBool>,
    // Tracks committed output that still needs durable reconciliation.
    publication_recovery: AtomicPublicationRecovery,
    // IndexFactory for creating storage backend indexes
    index_factory: Arc<crate::indexes::IndexFactory>,
    // Middleware registry for query middleware
    middleware_registry: Arc<MiddlewareTypeRegistry>,
    // FutureQueueSource for temporal query support
    future_queue_source: Arc<RwLock<Option<Arc<FutureQueueSource>>>>,
    // Persisted checkpoint_store across stop/start cycles for checkpoint recovery
    checkpoint_store: Arc<RwLock<Option<Arc<dyn CheckpointStore>>>>,
    // Persistent outbox writer for reaction replay (from index backend)
    outbox_writer: Arc<RwLock<Option<Arc<dyn OutboxWriter>>>>,
    // Persistent live results writer for snapshot recovery (from index backend)
    live_results_writer: Arc<RwLock<Option<Arc<dyn LiveResultsWriter>>>>,
    // Configurable bootstrap timeout for fetch APIs
    bootstrap_timeout: std::time::Duration,
    // Resolved recovery policy: per-query → global default → Strict
    resolved_recovery_policy: crate::recovery::RecoveryPolicy,
    // Track which source IDs we subscribed to, for cleanup in stop()
    subscribed_source_ids: Arc<RwLock<Vec<String>>>,
    // Per-query output metrics (outbox, sequence, snapshot health)
    output_metrics: Arc<QueryOutputMetrics>,
    #[cfg(test)]
    processing_observer: Arc<RwLock<Option<Arc<dyn QueryProcessingObserver>>>>,
}

impl DrasiQuery {
    pub fn new(
        instance_id: impl Into<String>,
        config: QueryConfig,
        source_manager: Arc<SourceManager>,
        index_factory: Arc<crate::indexes::IndexFactory>,
        middleware_registry: Arc<MiddlewareTypeRegistry>,
        default_recovery_policy: Option<crate::recovery::RecoveryPolicy>,
    ) -> Result<Self> {
        // Create priority queue with configured capacity (fallback to 10000 if not set)
        let priority_capacity = config.priority_queue_capacity.unwrap_or(10000);
        let priority_queue = PriorityQueue::new(priority_capacity);
        let ingress_fence = QueryIngressFence::new(priority_queue.clone());
        let outbox_capacity = config.outbox_capacity;
        let bootstrap_timeout = std::time::Duration::from_secs(config.bootstrap_timeout_secs);
        let config_hash = crate::queries::compute_config_hash(&config);

        // Resolve recovery policy: per-query → global default → Strict
        let resolved_recovery_policy = config
            .recovery_policy
            .or(default_recovery_policy)
            .unwrap_or_default();

        // Create QueryBase for common functionality
        let base = QueryBase::new(config).context("Failed to create QueryBase")?;

        Ok(Self {
            instance_id: instance_id.into(),
            base,
            output_state: Arc::new(RwLock::new(QueryOutputState::new(outbox_capacity))),
            config_hash,
            priority_queue,
            source_manager,
            ingress_fence,
            lifecycle_lock: Arc::new(Mutex::new(())),
            startup_cancel: Arc::new(StdMutex::new(None)),
            stop_requested: Arc::new(AtomicBool::new(false)),
            publication_recovery: AtomicPublicationRecovery::default(),
            index_factory,
            middleware_registry,
            future_queue_source: Arc::new(RwLock::new(None)),
            checkpoint_store: Arc::new(RwLock::new(None)),
            outbox_writer: Arc::new(RwLock::new(None)),
            live_results_writer: Arc::new(RwLock::new(None)),
            bootstrap_timeout,
            resolved_recovery_policy,
            subscribed_source_ids: Arc::new(RwLock::new(Vec::new())),
            output_metrics: Arc::new(QueryOutputMetrics::new()),
            #[cfg(test)]
            processing_observer: Arc::new(RwLock::new(None)),
        })
    }

    /// Initialize the query with runtime context.
    ///
    /// Wires the status handle to the component graph, following the same
    /// pattern as Source and Reaction initialization.
    pub async fn initialize(&self, context: crate::context::QueryRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn stop_future_queue_source(&self) {
        if let Some(source) = self.future_queue_source.write().await.take() {
            source.stop().await;
        }
    }

    async fn release_position_handles(&self) {
        let source_ids = {
            let mut source_ids = self.subscribed_source_ids.write().await;
            std::mem::take(&mut *source_ids)
        };
        for source_id in source_ids {
            if let Some(source) = self.source_manager.get_source_instance(&source_id).await {
                source.remove_position_handle(&self.base.config.id).await;
                debug!(
                    "Query '{}' released position handle for source '{}'",
                    self.base.config.id, source_id
                );
            }
        }
    }

    async fn reap_previous_runtime(&self) {
        self.ingress_fence.close().await;
        self.stop_future_queue_source().await;
        self.base.reap_task().await;
        self.release_position_handles().await;
    }

    fn cancel_startup(&self) {
        if let Some(cancel) = self
            .startup_cancel
            .lock()
            .expect("query startup cancellation lock poisoned")
            .take()
        {
            let _ = cancel.send(());
        }
    }

    pub async fn get_current_results(&self) -> Vec<serde_json::Value> {
        self.output_state.read().await.get_results_as_vec()
    }

    /// Wait until the query has finished bootstrapping (status is no longer `Starting`).
    ///
    /// Returns `Ok(())` if the query reaches `Running` status.
    /// Returns `Err(FetchError::NotRunning)` if it reaches a terminal non-Running state.
    /// Returns `Err(FetchError::TimedOut)` if bootstrap doesn't complete within the
    /// configured `bootstrap_timeout_secs`.
    async fn wait_until_running(&self) -> Result<(), FetchError> {
        let mut status_rx = self.base.status_handle().subscribe_status();

        // Check current value first (avoids waiting if already transitioned)
        let current = *status_rx.borrow_and_update();
        match current {
            ComponentStatus::Running => return Ok(()),
            ComponentStatus::Starting => {} // need to wait
            other => return Err(FetchError::NotRunning { status: other }),
        }

        // Wait for a non-Starting status, with timeout
        let result = tokio::time::timeout(
            self.bootstrap_timeout,
            status_rx.wait_for(|s| *s != ComponentStatus::Starting),
        )
        .await;

        match result {
            Ok(Ok(status_ref)) => {
                let status = *status_ref;
                if status == ComponentStatus::Running {
                    Ok(())
                } else {
                    Err(FetchError::NotRunning { status })
                }
            }
            Ok(Err(_)) => {
                // Watch channel closed — sender dropped, treat as not running
                Err(FetchError::NotRunning {
                    status: ComponentStatus::Stopped,
                })
            }
            Err(_) => Err(FetchError::TimedOut),
        }
    }
}

#[cfg(test)]
impl DrasiQuery {
    /// Count active subscription forwarder tasks (testing helper)
    pub async fn subscription_task_count(&self) -> usize {
        self.ingress_fence.task_count().await
    }

    /// Access the checkpoint store (for internal/test use only).
    #[doc(hidden)]
    pub async fn get_checkpoint_store(&self) -> Option<Arc<dyn CheckpointStore>> {
        self.checkpoint_store.read().await.clone()
    }

    pub(super) async fn set_processing_observer(
        &self,
        observer: Option<Arc<dyn QueryProcessingObserver>>,
    ) {
        *self.processing_observer.write().await = observer;
    }

    pub(crate) fn publication_recovery_required(&self) -> bool {
        self.publication_recovery.is_required()
    }

    pub(crate) async fn get_outbox_writer(&self) -> Option<Arc<dyn OutboxWriter>> {
        self.outbox_writer.read().await.clone()
    }

    pub(super) async fn set_local_status_for_test(&self, status: ComponentStatus) {
        self.base
            .set_status(status, Some("test transition".to_string()))
            .await;
    }

    pub(super) async fn output_sequence_for_test(&self) -> u64 {
        self.output_state.read().await.as_of_sequence()
    }
}

#[derive(Clone)]
struct PersistentQueryState {
    query_id: String,
    element_index: Option<Arc<dyn drasi_core::interface::ElementIndex>>,
    archive_index: Option<Arc<dyn drasi_core::interface::ElementArchiveIndex>>,
    result_index: Option<Arc<dyn drasi_core::interface::ResultIndex>>,
    future_queue: Option<Arc<dyn drasi_core::interface::FutureQueue>>,
    checkpoint_store: Arc<dyn CheckpointStore>,
    session_control: Arc<dyn drasi_core::interface::SessionControl>,
    outbox_writer: Option<Arc<dyn OutboxWriter>>,
    live_results_writer: Option<Arc<dyn LiveResultsWriter>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryRecoveryResetReason {
    ConfigChanged,
    ConfigHashReadFailed,
    IncompleteBootstrap,
    CheckpointReadFailed,
    InvalidBootstrapMarker,
    LegacyPublicationPending,
    OutputReconciliationFailed,
    PositionUnavailable,
    Deprovision,
}

impl PersistentQueryState {
    async fn clear_recovery_state(
        &self,
        output_state: &RwLock<QueryOutputState>,
        publication_recovery: &AtomicPublicationRecovery,
        config_hash: Option<u64>,
        reason: QueryRecoveryResetReason,
    ) -> anyhow::Result<()> {
        info!(
            "Query '{}' clearing persistent and process-local recovery state ({reason:?})",
            self.query_id
        );
        let preserved_sequence = if reason == QueryRecoveryResetReason::Deprovision {
            None
        } else {
            Some(
                self.safe_output_high_water(output_state, publication_recovery)
                    .await?,
            )
        };
        self.clear_persistent(config_hash, preserved_sequence)
            .await?;
        output_state
            .write()
            .await
            .reset_to_sequence(preserved_sequence.unwrap_or(0));
        publication_recovery.reconcile_in_memory();
        Ok(())
    }

    async fn safe_output_high_water(
        &self,
        output_state: &RwLock<QueryOutputState>,
        publication_recovery: &AtomicPublicationRecovery,
    ) -> anyhow::Result<u64> {
        let in_memory = output_state.read().await.as_of_sequence();
        let durable = self
            .checkpoint_store
            .read_result_sequence(&self.query_id)
            .await
            .context("failed to read durable result sequence before reset")?
            .unwrap_or(0);
        let outbox = match &self.outbox_writer {
            Some(writer) => writer
                .read_latest_sequence(&self.query_id)
                .await
                .context("failed to read durable outbox high-water before reset")?
                .unwrap_or(0),
            None => 0,
        };
        let atomic_pending = publication_recovery.pending_sequence().unwrap_or(0);
        let legacy_pending = match read_legacy_pending_state(self.checkpoint_store.as_ref()).await?
        {
            LegacyPendingState::Absent => 0,
            LegacyPendingState::Pending { output_sequence } => output_sequence.unwrap_or(0),
        };
        let reset_baseline = read_output_reset_high_water(self.checkpoint_store.as_ref())
            .await?
            .unwrap_or(0);
        Ok(in_memory
            .max(durable)
            .max(outbox)
            .max(atomic_pending)
            .max(legacy_pending)
            .max(reset_baseline))
    }

    async fn clear_persistent(
        &self,
        config_hash: Option<u64>,
        preserved_sequence: Option<u64>,
    ) -> anyhow::Result<()> {
        clear_persistent_indexes(
            &self.query_id,
            &self.element_index,
            &self.archive_index,
            &self.result_index,
            &self.future_queue,
        )
        .await?;

        if let Some(writer) = &self.outbox_writer {
            writer
                .clear(&self.query_id)
                .await
                .context("failed to clear the durable query outbox")?;
        }
        if let Some(writer) = &self.live_results_writer {
            writer
                .clear(&self.query_id)
                .await
                .context("failed to clear durable live results")?;
        }
        self.checkpoint_store
            .clear_checkpoints()
            .await
            .context("failed to clear query checkpoints")?;
        if let Some(preserved_sequence) = preserved_sequence {
            let session = drasi_core::interface::SessionGuard::begin(self.session_control.clone())
                .await
                .context("failed to begin output reset-baseline transaction")?;
            self.checkpoint_store
                .stage_checkpoint(QUERY_OUTPUT_RESET_MARKER_V1, preserved_sequence, None)
                .await
                .context("failed to stage output reset baseline")?;
            self.checkpoint_store
                .write_result_sequence(&self.query_id, preserved_sequence)
                .await
                .context("failed to preserve durable result sequence")?;
            session
                .commit()
                .await
                .context("failed to commit output reset baseline")?;
        } else {
            self.checkpoint_store
                .write_result_sequence(&self.query_id, 0)
                .await
                .context("failed to clear durable result sequence")?;
        }
        if let Some(config_hash) = config_hash {
            self.checkpoint_store
                .write_config_hash(config_hash)
                .await
                .context("failed to write the query config hash after reset")?;
        }
        Ok(())
    }
}

/// Clear persistent core indexes. Durable query output and checkpoints are
/// cleared by [`PersistentQueryState::clear`] in the same reset workflow.
async fn clear_persistent_indexes(
    query_id: &str,
    element_index: &Option<Arc<dyn drasi_core::interface::ElementIndex>>,
    archive_index: &Option<Arc<dyn drasi_core::interface::ElementArchiveIndex>>,
    result_index: &Option<Arc<dyn drasi_core::interface::ResultIndex>>,
    future_queue: &Option<Arc<dyn drasi_core::interface::FutureQueue>>,
) -> anyhow::Result<()> {
    use drasi_core::interface::IndexError;

    if let Some(ei) = element_index {
        match ei.clear().await {
            Ok(()) | Err(IndexError::NotSupported) => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Query '{query_id}' failed to clear element index"
                ))
                .context(format!("{e:?}"));
            }
        }
    }
    if let Some(ai) = archive_index {
        match ai.clear().await {
            Ok(()) | Err(IndexError::NotSupported) => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Query '{query_id}' failed to clear archive index"
                ))
                .context(format!("{e:?}"));
            }
        }
    }
    if let Some(ri) = result_index {
        match ri.clear().await {
            Ok(()) | Err(IndexError::NotSupported) => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Query '{query_id}' failed to clear result index"
                ))
                .context(format!("{e:?}"));
            }
        }
    }
    if let Some(fq) = future_queue {
        match fq.clear().await {
            Ok(()) | Err(IndexError::NotSupported) => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Query '{query_id}' failed to clear future queue"
                ))
                .context(format!("{e:?}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod recovery_reset_tests {
    use super::*;
    use drasi_core::interface::{IndexBackendPlugin, RowMutation};
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use serde_json::json;

    #[tokio::test]
    async fn every_destructive_reset_clears_output_and_publication_recovery() {
        let reasons = [
            QueryRecoveryResetReason::ConfigChanged,
            QueryRecoveryResetReason::ConfigHashReadFailed,
            QueryRecoveryResetReason::IncompleteBootstrap,
            QueryRecoveryResetReason::CheckpointReadFailed,
            QueryRecoveryResetReason::InvalidBootstrapMarker,
            QueryRecoveryResetReason::LegacyPublicationPending,
            QueryRecoveryResetReason::OutputReconciliationFailed,
            QueryRecoveryResetReason::PositionUnavailable,
            QueryRecoveryResetReason::Deprovision,
        ];

        let temp_dir = tempfile::TempDir::new().expect("create reset temp directory");
        let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);
        for (index, reason) in reasons.into_iter().enumerate() {
            let query_id = format!("reset-reason-{index}");
            let created = provider
                .create_indexes(&query_id)
                .await
                .expect("create reset indexes");
            let checkpoint_store = created
                .checkpoint_store
                .as_ref()
                .expect("checkpoint store")
                .clone();
            let outbox_writer = created
                .outbox_writer
                .as_ref()
                .expect("outbox writer")
                .clone();
            let live_results_writer = created
                .live_results_writer
                .as_ref()
                .expect("live-results writer")
                .clone();
            let result = Arc::new(QueryResult::new(
                query_id.clone(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: json!({ "name": "stale" }),
                    row_signature: 7,
                }],
                HashMap::new(),
            ));
            outbox_writer
                .append(
                    &query_id,
                    1,
                    &rmp_serde::to_vec(result.as_ref()).expect("serialize stale output"),
                )
                .await
                .expect("seed stale outbox");
            let row =
                rmp_serde::to_vec(&json!({ "name": "stale" })).expect("serialize stale live row");
            live_results_writer
                .apply_mutations(
                    &query_id,
                    &[RowMutation {
                        row_signature: 7,
                        data: Some(&row),
                    }],
                )
                .await
                .expect("seed stale live row");
            checkpoint_store
                .write_result_sequence(&query_id, 1)
                .await
                .expect("seed stale result sequence");
            created
                .set
                .session_control
                .begin()
                .await
                .expect("begin stale marker transaction");
            checkpoint_store
                .stage_checkpoint(QUERY_BOOTSTRAP_MARKER_V1, 0, None)
                .await
                .expect("seed stale bootstrap marker");
            created
                .set
                .session_control
                .commit()
                .await
                .expect("commit stale marker");

            let output_state = Arc::new(RwLock::new(QueryOutputState::new(8)));
            output_state
                .write()
                .await
                .apply_committed_arc(result.clone())
                .expect("seed process-local output");
            let publication_recovery = AtomicPublicationRecovery::default();
            publication_recovery.require(result);
            let state = PersistentQueryState {
                query_id: query_id.clone(),
                element_index: Some(created.set.element_index.clone()),
                archive_index: Some(created.set.archive_index.clone()),
                result_index: Some(created.set.result_index.clone()),
                future_queue: Some(created.set.future_queue.clone()),
                checkpoint_store: checkpoint_store.clone(),
                session_control: created.set.session_control.clone(),
                outbox_writer: Some(outbox_writer.clone()),
                live_results_writer: Some(live_results_writer.clone()),
            };
            let config_hash = (reason != QueryRecoveryResetReason::Deprovision).then_some(99);

            state
                .clear_recovery_state(&output_state, &publication_recovery, config_hash, reason)
                .await
                .expect("clear recovery state");

            let preserved_sequence = (reason != QueryRecoveryResetReason::Deprovision).then_some(1);
            let output = output_state.read().await;
            assert_eq!(
                output.as_of_sequence(),
                preserved_sequence.unwrap_or(0),
                "{reason:?}"
            );
            assert_eq!(output.results_len(), 0, "{reason:?}");
            assert_eq!(output.outbox_len(), 0, "{reason:?}");
            drop(output);
            assert!(!publication_recovery.is_required(), "{reason:?}");
            assert!(
                outbox_writer
                    .read_from(&query_id, 0)
                    .await
                    .expect("read cleared outbox")
                    .is_empty(),
                "{reason:?}"
            );
            assert!(
                live_results_writer
                    .read_snapshot(&query_id)
                    .await
                    .expect("read cleared live rows")
                    .is_empty(),
                "{reason:?}"
            );
            assert_eq!(
                checkpoint_store
                    .read_result_sequence(&query_id)
                    .await
                    .expect("read reset sequence"),
                preserved_sequence.or(Some(0)),
                "{reason:?}"
            );
            assert_eq!(
                checkpoint_store
                    .read_checkpoint(QUERY_OUTPUT_RESET_MARKER_V1)
                    .await
                    .expect("read output reset marker")
                    .map(|marker| marker.sequence),
                preserved_sequence,
                "{reason:?}"
            );
            assert!(
                checkpoint_store
                    .read_checkpoint(QUERY_BOOTSTRAP_MARKER_V1)
                    .await
                    .expect("read cleared marker")
                    .is_none(),
                "{reason:?}"
            );
            assert_eq!(
                checkpoint_store
                    .read_config_hash()
                    .await
                    .expect("read reset config hash"),
                config_hash,
                "{reason:?}"
            );
        }
    }
}

impl DrasiQuery {
    async fn start_inner(&self) -> Result<()> {
        log_component_start("Query", &self.base.config.id);

        self.reap_previous_runtime().await;

        // Set Starting on the local status handle. The manager has already validated
        // and applied the Starting transition on the graph via validate_and_transition().
        // This local update is needed because internal query logic (e.g., the bootstrap
        // completion check at line ~983) reads the handle's local status to decide
        // whether to transition to Running.
        //
        // INVARIANT: The graph must already be in Starting state before this point.
        // The idempotency check in update_status_with_message() ensures the duplicate
        // Starting update sent via mpsc is safely ignored.
        debug_assert!(
            matches!(
                self.base.status_handle().get_status().await,
                ComponentStatus::Stopped | ComponentStatus::Error | ComponentStatus::Starting
            ),
            "DrasiQuery::start() called but local handle is not in expected pre-start state"
        );
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting query".to_string()),
            )
            .await;

        // Build and initialize the actual Drasi ContinuousQuery
        let query_str = self.base.config.query.clone();

        // Create a parser and function registry based on the query language
        let config = Arc::new(DefaultQueryConfig);
        let (parser, function_registry): (Arc<dyn QueryParser>, Arc<FunctionRegistry>) =
            match self.base.config.query_language {
                QueryLanguage::Cypher => {
                    debug!(
                        "Query '{}' using Cypher parser and function set",
                        self.base.config.id
                    );
                    (
                        Arc::new(CypherParser::new(config)),
                        Arc::new(FunctionRegistry::new()).with_cypher_function_set(),
                    )
                }
                QueryLanguage::GQL => {
                    debug!(
                        "Query '{}' using GQL parser and function set",
                        self.base.config.id
                    );
                    (
                        Arc::new(GQLParser::new(config)),
                        Arc::new(FunctionRegistry::new()).with_gql_function_set(),
                    )
                }
            };

        let mut builder =
            QueryBuilder::new(&query_str, parser).with_function_registry(function_registry);

        // Configure middleware registry and middleware
        builder = builder.with_middleware_registry(self.middleware_registry.clone());

        // Add all middleware configurations from config
        for mw in &self.base.config.middleware {
            builder = builder.with_source_middleware(Arc::new(mw.clone()));
        }

        // Configure source pipelines for all subscriptions
        for sub in &self.base.config.sources {
            builder = builder.with_source_pipeline(&sub.source_id, &sub.pipeline);
        }

        // Add joins if configured
        if let Some(joins) = &self.base.config.joins {
            debug!(
                "Query '{}' has {} configured joins",
                self.base.config.id,
                joins.len()
            );
            let drasi_joins: Vec<drasi_core::models::QueryJoin> =
                joins.iter().cloned().map(|j| j.into()).collect();
            builder = builder.with_joins(drasi_joins);
        }

        // Build indexes - either from configured backend or default in-memory.
        // Keep a reference to the checkpoint_store for persistence.
        // Reuse the persisted checkpoint_store across stop/start cycles so that
        // in-memory checkpoints survive restarts within the same process lifetime.
        let checkpoint_store: Arc<dyn CheckpointStore>;
        // Keep index references for potential clearing on config hash mismatch.
        let element_index: Option<Arc<dyn drasi_core::interface::ElementIndex>>;
        let archive_index: Option<Arc<dyn drasi_core::interface::ElementArchiveIndex>>;
        let result_index: Option<Arc<dyn drasi_core::interface::ResultIndex>>;
        let future_queue: Option<Arc<dyn drasi_core::interface::FutureQueue>>;
        let session_control: Option<Arc<dyn drasi_core::interface::SessionControl>>;
        let outbox_writer: Option<Arc<dyn OutboxWriter>>;
        let live_results_writer: Option<Arc<dyn LiveResultsWriter>>;
        let processing_mode: QueryProcessingMode;

        if let Some(backend_ref) = self
            .base
            .config
            .storage_backend
            .as_ref()
            .or_else(|| self.index_factory.default_backend())
        {
            debug!(
                "Query '{}' using storage backend: {:?}",
                self.base.config.id, backend_ref
            );
            let index_factory = self.index_factory.clone();

            // Drop the previous checkpoint store handle before re-opening.
            // For backends like RocksDB that hold an exclusive lock on the
            // data directory, the old handle must be released before we can
            // open a new one.  Checkpoint data is already persisted on disk.
            *self.checkpoint_store.write().await = None;
            *self.outbox_writer.write().await = None;
            *self.live_results_writer.write().await = None;

            let created = index_factory
                .build(backend_ref, &self.base.config.id)
                .await
                .context("Failed to build indexes")?;
            processing_mode = QueryProcessingMode::from_created_indexes(&created);
            debug!(
                "Query '{}' selected {} result processing",
                self.base.config.id,
                processing_mode.diagnostic_name()
            );

            // Use backend-provided checkpoint store, or create in-memory fallback
            checkpoint_store = match created.checkpoint_store.as_ref() {
                Some(store) => store.clone(),
                None => {
                    // Backend didn't provide one; reuse persisted or create new
                    let existing = self.checkpoint_store.read().await.clone();
                    existing.unwrap_or_else(|| Arc::new(InMemoryCheckpointStore::new()))
                }
            };

            // Store persistent writers if provided by the backend
            outbox_writer = created.outbox_writer.clone();
            live_results_writer = created.live_results_writer.clone();
            *self.outbox_writer.write().await = outbox_writer.clone();
            *self.live_results_writer.write().await = live_results_writer.clone();
            // Hold references for potential clearing before passing to builder
            element_index = Some(created.set.element_index.clone());
            archive_index = Some(created.set.archive_index.clone());
            result_index = Some(created.set.result_index.clone());
            future_queue = Some(created.set.future_queue.clone());
            session_control = Some(created.set.session_control.clone());

            builder = builder
                .with_element_index(created.set.element_index)
                .with_archive_index(created.set.archive_index)
                .with_result_index(created.set.result_index)
                .with_future_queue(created.set.future_queue)
                .with_session_control(created.set.session_control);
        } else {
            debug!(
                "Query '{}' using default in-memory indexes",
                self.base.config.id
            );
            // Reuse persisted checkpoint_store if available (e.g., after stop/restart)
            let existing = self.checkpoint_store.read().await.clone();
            checkpoint_store = existing.unwrap_or_else(|| Arc::new(InMemoryCheckpointStore::new()));
            element_index = None;
            archive_index = None;
            result_index = None;
            future_queue = None;
            session_control = None;
            outbox_writer = None;
            live_results_writer = None;
            processing_mode = QueryProcessingMode::legacy();
        };

        // Persist the checkpoint_store for future stop/start cycles
        *self.checkpoint_store.write().await = Some(checkpoint_store.clone());
        let persistent_state = PersistentQueryState {
            query_id: self.base.config.id.clone(),
            element_index: element_index.clone(),
            archive_index: archive_index.clone(),
            result_index: result_index.clone(),
            future_queue: future_queue.clone(),
            checkpoint_store: checkpoint_store.clone(),
            session_control: session_control
                .clone()
                .unwrap_or_else(|| Arc::new(drasi_core::interface::NoOpSessionControl)),
            outbox_writer: outbox_writer.clone(),
            live_results_writer: live_results_writer.clone(),
        };
        if let Err(error) = processing_mode.validate_legacy_pending_capability() {
            let message = format!(
                "Query '{}' cannot start persistent Legacy processing safely: {error:#}",
                self.base.config.id
            );
            self.base
                .set_status(ComponentStatus::Error, Some(message.clone()))
                .await;
            return Err(anyhow::anyhow!(message));
        }

        let continuous_query = match builder.try_build().await {
            Ok(query) => query,
            Err(e) => {
                error!("Failed to build query '{}': {}", self.base.config.id, e);
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Failed to build query: {e}")),
                    )
                    .await;

                return Err(anyhow::anyhow!("Failed to build query: {e}"));
            }
        };

        // Extract labels from the query for bootstrap
        let labels = match crate::queries::LabelExtractor::extract_labels(
            &query_str,
            &self.base.config.query_language,
        ) {
            Ok(labels) => labels,
            Err(e) => {
                warn!("Failed to extract labels from query '{}': {}. Bootstrap will request all data.",
                    self.base.config.id, e);
                crate::queries::QueryLabels {
                    node_labels: vec![],
                    relation_labels: vec![],
                }
            }
        };

        // Build subscription settings for each source
        let subscription_settings =
            match crate::queries::SubscriptionSettingsBuilder::build_subscription_settings(
                &self.base.config,
                &labels,
            ) {
                Ok(settings) => settings,
                Err(e) => {
                    error!(
                        "Failed to build subscription settings for query '{}': {}",
                        self.base.config.id, e
                    );
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Failed to build subscription settings: {e}")),
                        )
                        .await;

                    return Err(anyhow::anyhow!(
                        "Failed to build subscription settings: {e}"
                    ));
                }
            };
        if self.base.config.sources.iter().any(|source| {
            matches!(
                source.source_id.as_str(),
                QUERY_BOOTSTRAP_MARKER_V1
                    | QUERY_OUTPUT_RESET_MARKER_V1
                    | LEGACY_OUTPUT_PENDING_MARKER_V1
            )
        }) {
            let message = format!(
                "Query '{}' uses a source id reserved for bootstrap recovery",
                self.base.config.id
            );
            self.base
                .set_status(ComponentStatus::Error, Some(message.clone()))
                .await;
            return Err(anyhow::anyhow!(message));
        }

        // Read the last checkpoints and propagate source_position to subscription settings
        // so sources can resume from where they left off.
        //
        // Only propagate checkpoint recovery when the checkpoint store is persistent.
        // Volatile (in-memory) stores don't survive restarts, and their paired element
        // indexes rebuild fresh on each start — bootstrap must run to populate the
        // graph state. Skipping bootstrap against an empty graph would produce
        // incorrect results.
        let mut subscription_settings = subscription_settings;
        let has_persistent_backend = checkpoint_store.is_persistent();
        let mut checkpoint_sequences_per_source: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        if has_persistent_backend {
            let current_hash = super::compute_config_hash(&self.base.config);
            let config_matches = match checkpoint_store.read_config_hash().await {
                Ok(Some(stored_hash)) if stored_hash == current_hash => {
                    debug!(
                        "Query '{}' config hash matches stored hash ({current_hash}), resuming",
                        self.base.config.id
                    );
                    true
                }
                Ok(Some(stored_hash)) => {
                    info!(
                        "Query '{}' config hash changed ({stored_hash} -> {current_hash}), clearing all persistent state for full bootstrap",
                        self.base.config.id
                    );
                    if let Err(e) = persistent_state
                        .clear_recovery_state(
                            &self.output_state,
                            &self.publication_recovery,
                            Some(current_hash),
                            QueryRecoveryResetReason::ConfigChanged,
                        )
                        .await
                    {
                        let msg = format!(
                            "Query '{}' failed to clear persistent state on config change: {e:#}",
                            self.base.config.id
                        );
                        error!("{msg}");
                        self.base
                            .set_status(ComponentStatus::Error, Some(msg.clone()))
                            .await;
                        return Err(anyhow::anyhow!(msg));
                    }
                    false
                }
                Ok(None) => {
                    info!(
                        "Query '{}' no stored config hash (first run), writing hash {current_hash}",
                        self.base.config.id
                    );
                    if let Err(e) = checkpoint_store.write_config_hash(current_hash).await {
                        warn!(
                            "Query '{}' failed to write config hash: {e}",
                            self.base.config.id
                        );
                    }
                    false
                }
                Err(e) => {
                    warn!(
                        "Query '{}' failed to read config hash, clearing persistent state and starting fresh: {e}",
                        self.base.config.id
                    );
                    if let Err(reset_error) = persistent_state
                        .clear_recovery_state(
                            &self.output_state,
                            &self.publication_recovery,
                            Some(current_hash),
                            QueryRecoveryResetReason::ConfigHashReadFailed,
                        )
                        .await
                    {
                        let msg = format!(
                            "Query '{}' failed to clear persistent state after config hash read failure: {reset_error:#}",
                            self.base.config.id
                        );
                        error!("{msg}");
                        self.base
                            .set_status(ComponentStatus::Error, Some(msg.clone()))
                            .await;
                        return Err(anyhow::anyhow!(msg));
                    }
                    false
                }
            };

            if config_matches {
                let recovery_state =
                    QueryBootstrapScope::read_recovery_state(checkpoint_store.as_ref()).await;
                match recovery_state {
                    Ok(QueryBootstrapRecoveryState::InProgress) => {
                        info!(
                            "Query '{}' found an incomplete bootstrap marker; clearing partial state before retry",
                            self.base.config.id
                        );
                        persistent_state
                            .clear_recovery_state(
                                &self.output_state,
                                &self.publication_recovery,
                                Some(current_hash),
                                QueryRecoveryResetReason::IncompleteBootstrap,
                            )
                            .await
                            .context("failed to clear an incomplete bootstrap")?;
                    }
                    Ok(
                        QueryBootstrapRecoveryState::Absent | QueryBootstrapRecoveryState::Complete,
                    ) => match checkpoint_store.read_all_checkpoints().await {
                        Ok(checkpoints) => {
                            for settings in &mut subscription_settings {
                                if let Some(cp) = checkpoints.get(&settings.source_id) {
                                    checkpoint_sequences_per_source
                                        .insert(settings.source_id.clone(), cp.sequence);
                                    settings.request_position_handle = true;
                                    settings.resume_sequence = Some(cp.sequence);
                                    if let Some(pos) = &cp.source_position {
                                        settings.resume_from = Some(pos.clone());
                                    }
                                    debug!(
                                        "Query '{}' resuming source '{}' from checkpoint: seq={}",
                                        self.base.config.id, settings.source_id, cp.sequence
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            let detail = format!(
                                "Query '{}' failed to read durable checkpoints: {error}",
                                self.base.config.id
                            );
                            match self.resolved_recovery_policy {
                                crate::recovery::RecoveryPolicy::Strict => {
                                    self.base
                                        .set_status(ComponentStatus::Error, Some(detail.clone()))
                                        .await;
                                    return Err(anyhow::anyhow!(detail));
                                }
                                crate::recovery::RecoveryPolicy::AutoReset => {
                                    warn!("{detail}; AutoReset is clearing persistent state");
                                    persistent_state
                                        .clear_recovery_state(
                                            &self.output_state,
                                            &self.publication_recovery,
                                            Some(current_hash),
                                            QueryRecoveryResetReason::CheckpointReadFailed,
                                        )
                                        .await?;
                                }
                            }
                        }
                    },
                    Err(error) => {
                        let detail = format!(
                            "Query '{}' has an invalid bootstrap recovery marker: {error:#}",
                            self.base.config.id
                        );
                        match self.resolved_recovery_policy {
                            crate::recovery::RecoveryPolicy::Strict => {
                                self.base
                                    .set_status(ComponentStatus::Error, Some(detail.clone()))
                                    .await;
                                return Err(anyhow::anyhow!(detail));
                            }
                            crate::recovery::RecoveryPolicy::AutoReset => {
                                warn!("{detail}; AutoReset is clearing persistent state");
                                persistent_state
                                    .clear_recovery_state(
                                        &self.output_state,
                                        &self.publication_recovery,
                                        Some(current_hash),
                                        QueryRecoveryResetReason::InvalidBootstrapMarker,
                                    )
                                    .await?;
                            }
                        }
                    }
                };
            }

            for settings in &mut subscription_settings {
                settings.request_position_handle = true;
            }
        }

        if let LegacyPendingState::Pending { output_sequence } =
            processing_mode.read_legacy_pending_state().await?
        {
            let detail = format!(
                "Query '{}' has an unfinished persistent Legacy publication \
                 (output sequence {output_sequence:?})",
                self.base.config.id
            );
            match self.resolved_recovery_policy {
                crate::recovery::RecoveryPolicy::Strict => {
                    self.base
                        .set_status(ComponentStatus::Error, Some(detail.clone()))
                        .await;
                    return Err(anyhow::anyhow!(detail));
                }
                crate::recovery::RecoveryPolicy::AutoReset => {
                    warn!("{detail}; AutoReset is clearing persistent state");
                    persistent_state
                        .clear_recovery_state(
                            &self.output_state,
                            &self.publication_recovery,
                            Some(super::compute_config_hash(&self.base.config)),
                            QueryRecoveryResetReason::OutputReconciliationFailed,
                        )
                        .await?;
                    checkpoint_sequences_per_source.clear();
                    for settings in &mut subscription_settings {
                        settings.resume_from = None;
                        settings.resume_sequence = None;
                    }
                }
            }
        }

        let output_dependencies = QueryOutputDependencies::new(
            self.base.config.id.clone(),
            self.output_state.clone(),
            self.base.dispatchers.clone(),
            outbox_writer.clone(),
            live_results_writer.clone(),
            Some(checkpoint_store.clone()),
            self.output_state.read().await.outbox_capacity(),
            self.output_metrics.clone(),
        )
        .with_publication_recovery(self.publication_recovery.clone());
        #[cfg(test)]
        let output_dependencies = match self.processing_observer.read().await.clone() {
            Some(observer) => output_dependencies.with_observer(observer),
            None => output_dependencies,
        };

        if let Err(error) =
            QueryCompositeHost::reconcile_output_state(&processing_mode, &output_dependencies).await
        {
            let detail = format!(
                "Query '{}' durable output reconciliation failed: {error:#}",
                self.base.config.id
            );
            match self.resolved_recovery_policy {
                crate::recovery::RecoveryPolicy::Strict => {
                    self.base
                        .set_status(ComponentStatus::Error, Some(detail.clone()))
                        .await;
                    return Err(anyhow::anyhow!(detail));
                }
                crate::recovery::RecoveryPolicy::AutoReset => {
                    warn!("{detail}; AutoReset is clearing persistent state");
                    persistent_state
                        .clear_recovery_state(
                            &self.output_state,
                            &self.publication_recovery,
                            Some(super::compute_config_hash(&self.base.config)),
                            QueryRecoveryResetReason::OutputReconciliationFailed,
                        )
                        .await?;
                    checkpoint_sequences_per_source.clear();
                    for settings in &mut subscription_settings {
                        settings.resume_from = None;
                        settings.resume_sequence = None;
                    }
                    QueryCompositeHost::reconcile_output_state(
                        &processing_mode,
                        &output_dependencies,
                    )
                    .await
                    .context("durable output reconciliation still failed after AutoReset")?;
                }
            }
        }

        // Set up FutureQueueSource for temporal query support.
        // This creates a virtual source that polls the future queue and emits
        // FuturesDue control signals, integrating temporal queries into the
        // standard source subscription mechanism.
        debug!(
            "Query '{}' setting up FutureQueueSource for temporal queries",
            self.base.config.id
        );

        let future_queue_source = Arc::new(FutureQueueSource::new(
            continuous_query.future_queue(),
            self.base.config.id.clone(),
        ));

        // Subscribe BEFORE starting so the dispatcher exists when the polling loop runs
        let fq_receiver = future_queue_source
            .subscribe()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe to FutureQueueSource: {e}"))?;

        // Store for lifecycle cleanup in stop()
        *self.future_queue_source.write().await = Some(Arc::clone(&future_queue_source));

        info!(
            "Query '{}' subscribing to {} sources: {:?}",
            self.base.config.id,
            self.base.config.sources.len(),
            self.base
                .config
                .sources
                .iter()
                .map(|s| &s.source_id)
                .collect::<Vec<_>>()
        );

        let mut bootstrap_channels = Vec::new();
        // Build list of sources to subscribe to
        let mut sources_to_subscribe: Vec<(String, Arc<dyn Source>, SourceSubscriptionSettings)> =
            Vec::new();

        // Add regular sources from SourceManager
        for (idx, subscription) in self.base.config.sources.iter().enumerate() {
            let source_id = &subscription.source_id;
            match self.source_manager.get_source_instance(source_id).await {
                Some(src) => {
                    sources_to_subscribe.push((
                        source_id.clone(),
                        src,
                        subscription_settings[idx].clone(),
                    ));
                }
                None => {
                    error!(
                        "Query '{}' failed to find source '{}' in SourceManager",
                        self.base.config.id, source_id
                    );
                    // Cleanup already-spawned tasks before returning error
                    self.ingress_fence.close().await;
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Source '{source_id}' not found")),
                        )
                        .await;
                    return Err(crate::managers::ComponentNotFoundError::new(
                        "source",
                        source_id.as_str(),
                    )
                    .into());
                }
            }
        }

        // Compatibility validation: persistent queries must not use volatile sources.
        // A volatile source (supports_replay() == false) cannot guarantee event replay
        // after a restart, so resuming from checkpoints could produce incorrect results
        // (gaps in the data stream).
        if has_persistent_backend {
            let volatile_sources: Vec<&str> = sources_to_subscribe
                .iter()
                .filter(|(_, src, _)| !src.supports_replay())
                .map(|(id, _, _)| id.as_str())
                .collect();
            if !volatile_sources.is_empty() {
                let reason = format!(
                    "source(s) {volatile_sources:?} do not support replay; checkpoint-based recovery requires durable sources"
                );
                let msg = format!(
                    "Query '{}' has a persistent backend but {reason}",
                    self.base.config.id
                );
                error!("{msg}");
                self.base
                    .set_status(ComponentStatus::Error, Some(msg))
                    .await;
                return Err(crate::recovery::RecoveryError::IncompatibleSource {
                    query_id: self.base.config.id.clone(),
                    source_id: volatile_sources.join(", "),
                    reason,
                }
                .into());
            }
        }

        // Register every intended source before the first cancellable subscribe.
        // Stop/restart cleanup can then remove partial-start position/replay state.
        *self.subscribed_source_ids.write().await = sources_to_subscribe
            .iter()
            .map(|(id, _, _)| id.clone())
            .collect();

        let mut position_handles: std::collections::HashMap<
            String,
            Arc<std::sync::atomic::AtomicU64>,
        > = std::collections::HashMap::new();

        // Subscribe to all sources. If a PositionUnavailable error occurs and
        // the AutoReset policy is active, we clear all persistent state and
        // retry the entire loop with resume_from cleared to trigger full
        // re-bootstrap. The retry runs at most once.
        let mut auto_reset_retry = false;

        'subscribe_loop: loop {
            // On AutoReset retry: clear resume positions so sources bootstrap from scratch.
            if auto_reset_retry {
                info!(
                    "Query '{}' auto-reset: clearing resume positions and re-subscribing all sources",
                    self.base.config.id
                );
                for (_, _, settings) in &mut sources_to_subscribe {
                    settings.resume_from = None;
                    settings.resume_sequence = None;
                    settings.request_position_handle = has_persistent_backend;
                }
                // Reset per-loop accumulators
                bootstrap_channels.clear();
                self.ingress_fence.close().await;
                position_handles.clear();
                checkpoint_sequences_per_source.clear();
            }

            for (source_id, source, settings) in &sources_to_subscribe {
                let subscription_response = match source.subscribe(settings.clone()).await {
                    Ok(response) => response,
                    Err(e) => {
                        // Check if this is a PositionUnavailable error (gap detection)
                        if let Some(source_err) = e.downcast_ref::<crate::sources::SourceError>() {
                            match source_err {
                                crate::sources::SourceError::PositionUnavailable { .. } => {
                                    match self.resolved_recovery_policy {
                                        crate::recovery::RecoveryPolicy::Strict => {
                                            let msg = format!(
                                                "Query '{}' source '{}' cannot resume from checkpoint position (Strict policy): {e}",
                                                self.base.config.id, source_id
                                            );
                                            error!("{msg}");
                                            // Cleanup already-spawned tasks
                                            self.ingress_fence.close().await;
                                            // Release position handles for already-subscribed sources
                                            for (sid, _, _) in &sources_to_subscribe {
                                                if let Some(src) = self
                                                    .source_manager
                                                    .get_source_instance(sid)
                                                    .await
                                                {
                                                    src.remove_position_handle(
                                                        &self.base.config.id,
                                                    )
                                                    .await;
                                                }
                                            }
                                            self.base
                                                .set_status(ComponentStatus::Error, Some(msg))
                                                .await;
                                            return Err(e.context(format!(
                                                "PositionUnavailable for source '{source_id}' with Strict recovery policy"
                                            )));
                                        }
                                        crate::recovery::RecoveryPolicy::AutoReset => {
                                            if auto_reset_retry {
                                                // Already retried once — don't loop forever
                                                let msg = format!(
                                                    "Query '{}' auto-reset retry failed for source '{}': {e}",
                                                    self.base.config.id, source_id
                                                );
                                                error!("{msg}");
                                                self.ingress_fence.close().await;
                                                // Release position handles for already-subscribed sources
                                                for (sid, _, _) in &sources_to_subscribe {
                                                    if let Some(src) = self
                                                        .source_manager
                                                        .get_source_instance(sid)
                                                        .await
                                                    {
                                                        src.remove_position_handle(
                                                            &self.base.config.id,
                                                        )
                                                        .await;
                                                    }
                                                }
                                                self.base
                                                    .set_status(ComponentStatus::Error, Some(msg))
                                                    .await;
                                                return Err(e.context(
                                                    "AutoReset retry failed with PositionUnavailable",
                                                ));
                                            }

                                            warn!(
                                                "Query '{}' source '{}' position unavailable — AutoReset: wiping persistent state and re-bootstrapping all sources",
                                                self.base.config.id, source_id
                                            );

                                            // Abort already-spawned subscription tasks from this loop iteration
                                            self.ingress_fence.close().await;

                                            // Drain queued events so stale pre-reset events don't
                                            // get processed after re-bootstrap.
                                            let drained = self.priority_queue.drain().await;
                                            if !drained.is_empty() {
                                                debug!(
                                                    "Query '{}' auto-reset: drained {} stale events from priority queue",
                                                    self.base.config.id, drained.len()
                                                );
                                            }

                                            // Release position handles for sources that subscribed
                                            // before the failure, so they can advance their watermark.
                                            for (sid, _, _) in &sources_to_subscribe {
                                                if let Some(src) = self
                                                    .source_manager
                                                    .get_source_instance(sid)
                                                    .await
                                                {
                                                    src.remove_position_handle(
                                                        &self.base.config.id,
                                                    )
                                                    .await;
                                                }
                                            }

                                            // Clear every core and output artifact. If clearing
                                            // fails, abort rather than mixing old and fresh state.
                                            if has_persistent_backend {
                                                if let Err(reset_error) = persistent_state
                                                    .clear_recovery_state(
                                                        &self.output_state,
                                                        &self.publication_recovery,
                                                        Some(super::compute_config_hash(
                                                            &self.base.config,
                                                        )),
                                                        QueryRecoveryResetReason::PositionUnavailable,
                                                    )
                                                    .await
                                                {
                                                    let msg = format!(
                                                        "Query '{}' auto-reset failed while clearing persistent state: {reset_error:#}",
                                                        self.base.config.id
                                                    );
                                                    error!("{msg}");
                                                    self.base
                                                        .set_status(
                                                            ComponentStatus::Error,
                                                            Some(msg),
                                                        )
                                                        .await;
                                                    return Err(anyhow::anyhow!(
                                                        "AutoReset aborted: {reset_error:#}",
                                                    ));
                                                }
                                            }

                                            auto_reset_retry = true;
                                            continue 'subscribe_loop;
                                        }
                                    }
                                }
                            }
                        }

                        // Generic (non-PositionUnavailable) subscribe error
                        error!(
                            "Query '{}' failed to subscribe to source '{}': {}",
                            self.base.config.id, source_id, e
                        );
                        // Cleanup already-spawned tasks before returning error
                        self.ingress_fence.close().await;
                        // Release position handles for already-subscribed sources
                        for (sid, _, _) in &sources_to_subscribe {
                            if let Some(src) = self.source_manager.get_source_instance(sid).await {
                                src.remove_position_handle(&self.base.config.id).await;
                            }
                        }
                        self.base
                            .set_status(
                                ComponentStatus::Error,
                                Some(format!("Failed to subscribe to source '{source_id}': {e}")),
                            )
                            .await;
                        return Err(anyhow::anyhow!(
                            "Failed to subscribe to source '{source_id}': {e}"
                        ));
                    }
                };

                info!(
                    "Query '{}' successfully subscribed to source '{}'",
                    self.base.config.id, source_id
                );

                // Store bootstrap channel if provided
                if let Some(bootstrap_rx) = subscription_response.bootstrap_receiver {
                    bootstrap_channels.push(QueryBootstrapInput::new(
                        source_id.clone(),
                        bootstrap_rx,
                        subscription_response.bootstrap_result_receiver,
                    ));
                }

                // Collect position handle if source provides one
                if let Some(handle) = subscription_response.position_handle {
                    // Seed the handle with the query's checkpoint sequence (if
                    // resuming) so the source includes this subscriber in its
                    // min-watermark from the start. Without this, a resuming
                    // query whose handle stays at u64::MAX would be invisible to
                    // the min-watermark, letting upstream advance past its
                    // checkpoint. First-run queries (no checkpoint) leave the
                    // handle at u64::MAX ("no position confirmed yet").
                    if let Some(seq) = checkpoint_sequences_per_source.get(source_id) {
                        handle.store(*seq, std::sync::atomic::Ordering::Release);
                    }
                    position_handles.insert(source_id.clone(), handle);
                }

                // Spawn task to forward events from receiver to priority queue
                let mut receiver = subscription_response.receiver;
                let priority_queue = self.priority_queue.clone();
                let query_id = self.base.config.id.clone();
                let source_id_clone = source_id.clone();
                let instance_id = self.instance_id.clone();

                // Get source dispatch mode to determine enqueue strategy
                let dispatch_mode = source.dispatch_mode();
                let use_blocking_enqueue =
                    matches!(dispatch_mode, crate::channels::DispatchMode::Channel);

                let span = tracing::info_span!(
                    "query_source_forwarder",
                    instance_id = %instance_id,
                    component_id = %query_id,
                    component_type = "query"
                );
                let task = tokio::spawn(
                    async move {
                        debug!(
                            "Query '{query_id}' started event forwarder for source '{source_id_clone}' (dispatch_mode: {dispatch_mode:?}, blocking_enqueue: {use_blocking_enqueue})"
                        );

                        loop {
                            match receiver.recv().await {
                                Ok(arc_event) => {
                                    // Use appropriate enqueue method based on dispatch mode
                                    if use_blocking_enqueue {
                                        // Channel mode: Use blocking enqueue to prevent message loss
                                        // This creates backpressure when the priority queue is full
                                        priority_queue.enqueue_wait(arc_event).await;
                                    } else {
                                        // Broadcast mode: Use non-blocking enqueue to prevent deadlock
                                        // Messages may be dropped when priority queue is full
                                        if !priority_queue.enqueue(arc_event).await {
                                            warn!(
                                                "Query '{query_id}' priority queue at capacity, dropping event from source '{source_id_clone}' (broadcast mode)"
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "Query '{query_id}' receiver error for source '{source_id_clone}': {e}"
                                    );
                                    info!(
                                        "Query '{query_id}' channel closed for source '{source_id_clone}'"
                                    );
                                    break;
                                }
                            }
                        }

                        debug!("Query '{query_id}' event forwarder exited for source '{source_id_clone}'");
                    }
                    .instrument(span),
                );

                self.ingress_fence.push(task).await;
            }

            // All sources subscribed successfully — break out of the retry loop
            break;
        }

        // Wrap continuous_query in Arc for sharing across tasks
        let continuous_query = Arc::new(continuous_query);
        let bootstrap_scope = QueryBootstrapScope::new(
            bootstrap_channels,
            checkpoint_store.clone(),
            session_control.clone(),
        );

        // Spawn FutureQueueSource forwarder task (same pattern as other sources)
        {
            let fq_priority_queue = self.priority_queue.clone();
            let fq_forwarder = tokio::spawn(async move {
                let mut receiver = fq_receiver;
                while let Ok(event) = receiver.recv().await {
                    fq_priority_queue.enqueue_wait(event).await;
                }
            });
            self.ingress_fence.push(fq_forwarder).await;
        }

        let host = QueryCompositeHost::new(
            QueryHostRuntime::new(
                self.instance_id.clone(),
                self.base.config.id.clone(),
                self.priority_queue.clone(),
                self.base.status_handle(),
                future_queue_source,
                self.ingress_fence.clone(),
            ),
            bootstrap_scope,
            processing_mode,
            QueryLiveDependencies::new(
                continuous_query,
                checkpoint_store.clone(),
                checkpoint_sequences_per_source,
                position_handles,
            ),
            output_dependencies,
        );
        host.start(&self.base).await;

        Ok(())
    }
}

#[async_trait]
impl Query for DrasiQuery {
    async fn start(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        if self.stop_requested.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!(
                "Query '{}' startup was cancelled by stop",
                self.base.config.id
            ));
        }
        let current_status = self.base.status_handle().get_status().await;
        let processor_installed = self.base.task_handle.read().await.is_some();
        if processor_installed
            && matches!(
                current_status,
                ComponentStatus::Starting | ComponentStatus::Running
            )
        {
            debug!(
                "Query '{}' start is already in progress or complete",
                self.base.config.id
            );
            return Ok(());
        }
        if current_status == ComponentStatus::Stopping {
            return Err(anyhow::anyhow!(
                "Query '{}' cannot start while cleanup is in progress",
                self.base.config.id
            ));
        }
        let (_startup_registration, mut startup_cancel) =
            StartupCancellationRegistration::install(self.startup_cancel.clone());
        if self.stop_requested.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!(
                "Query '{}' startup was cancelled by stop",
                self.base.config.id
            ));
        }

        tokio::select! {
            biased;
            _ = &mut startup_cancel => {
                // The losing startup future has been dropped, so all locally
                // pending task guards have fired. Reap anything already
                // transferred to shared ownership before releasing the lock.
                self.reap_previous_runtime().await;
                Err(anyhow::anyhow!(
                    "Query '{}' startup was cancelled by stop",
                    self.base.config.id
                ))
            }
            result = self.start_inner() => result,
        }
    }

    async fn stop(&self) -> Result<()> {
        self.stop_requested.store(true, Ordering::Release);
        let _stop_request = StopRequestReset(self.stop_requested.clone());
        self.cancel_startup();
        let _lifecycle = self.lifecycle_lock.lock().await;
        log_component_stop("Query", &self.base.config.id);

        // Set Stopping on the local status handle. The manager has already validated
        // and applied the Stopping transition on the graph via validate_and_transition().
        // This local update is needed because the event processing loop checks the
        // handle's local status to decide when to exit.
        //
        // INVARIANT: The graph must already be in Stopping state before this point.
        debug_assert!(
            matches!(
                self.base.status_handle().get_status().await,
                ComponentStatus::Running
                    | ComponentStatus::Starting
                    | ComponentStatus::Stopping
                    | ComponentStatus::Error
                    | ComponentStatus::Stopped
            ),
            "DrasiQuery::stop() called but local handle is not in expected pre-stop state"
        );
        if self.base.status_handle().get_status().await != ComponentStatus::Stopped {
            self.base
                .set_status(
                    ComponentStatus::Stopping,
                    Some("Stopping query".to_string()),
                )
                .await;
        }

        self.ingress_fence.close().await;
        self.stop_future_queue_source().await;
        self.release_position_handles().await;

        // Finish shutting down the host-owned processor task through QueryBase.
        QueryCompositeHost::stop(&self.base).await?;

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Query stopped successfully".to_string()),
            )
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn get_config(&self) -> &QueryConfig {
        &self.base.config
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn subscription_count(&self) -> usize {
        self.ingress_fence.task_count().await
    }

    async fn subscribe(&self, reaction_id: String) -> Result<QuerySubscriptionResponse> {
        debug!(
            "Reaction '{}' subscribing to query '{}'",
            reaction_id, self.base.config.id
        );

        self.base
            .subscribe(&reaction_id)
            .await
            .context("Failed to subscribe to query")
    }

    async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
        // Block until bootstrap is complete (status transitions from Starting to Running).
        // This ensures reactions don't observe a partial result set during initialization.
        self.wait_until_running().await?;

        // Track snapshot fetch invocations
        self.output_metrics.record_snapshot_fetch();

        let (results_clone, as_of_sequence) = {
            let state = self.output_state.read().await;
            (state.clone_results(), state.as_of_sequence())
        };

        // If in-memory state has results, return them directly
        if !results_clone.is_empty() || as_of_sequence > 0 {
            return Ok(SnapshotResponse::new(
                results_clone,
                as_of_sequence,
                self.config_hash,
            ));
        }

        // In-memory state is empty at sequence 0 — try persistent live results
        let query_id = &self.base.config.id;
        let live_writer = self.live_results_writer.read().await;
        if let Some(writer) = live_writer.as_ref() {
            let cp_store = self.checkpoint_store.read().await;
            let persisted_seq = if let Some(store) = cp_store.as_ref() {
                match store.read_result_sequence(query_id).await {
                    Ok(Some(seq)) => seq,
                    Ok(None) => 0,
                    Err(e) => {
                        warn!("Query '{query_id}' failed to read persisted result sequence: {e}");
                        0
                    }
                }
            } else {
                0
            };

            if persisted_seq > 0 {
                match writer.read_snapshot(query_id).await {
                    Ok(rows) => {
                        let mut results = im::HashMap::new();
                        for (sig, data) in &rows {
                            match rmp_serde::from_slice::<serde_json::Value>(data) {
                                Ok(value) => {
                                    results.insert(*sig, value);
                                }
                                Err(e) => {
                                    warn!(
                                        "Query '{query_id}' failed to deserialize live results row (sig={sig}): {e}"
                                    );
                                }
                            }
                        }
                        // Return with persisted_seq even if rows is empty
                        // (all rows deleted is a valid state).
                        return Ok(SnapshotResponse::new(
                            results,
                            persisted_seq,
                            self.config_hash,
                        ));
                    }
                    Err(e) => {
                        warn!("Query '{query_id}' failed to read persistent live results: {e}");
                    }
                }
            }
        }

        // Nothing in persistent storage either — return empty
        Ok(SnapshotResponse::new(
            results_clone,
            as_of_sequence,
            self.config_hash,
        ))
    }

    async fn fetch_outbox(&self, after_sequence: u64) -> Result<OutboxResponse, FetchError> {
        // Block until bootstrap is complete — outbox is only populated by live processing.
        self.wait_until_running().await?;

        let state = self.output_state.read().await;
        let results = state
            .fetch_outbox_after(after_sequence)
            .map_err(|mut gap| {
                gap.config_hash = self.config_hash;
                gap
            })?;
        Ok(OutboxResponse {
            latest_sequence: state.as_of_sequence(),
            results,
            config_hash: self.config_hash,
        })
    }

    fn output_metrics(&self) -> Option<Arc<QueryOutputMetrics>> {
        Some(self.output_metrics.clone())
    }

    async fn release_persistent_handles(&self) {
        // Drop the persistent handles created by the index backend. For a shared
        // backend like RocksDB (one `OptimisticTransactionDB` cloned into every
        // index/store/writer), these are the only backend clones the query still
        // retains after `stop()`; dropping them lets the backend release its
        // exclusive lock. On-disk data is left untouched, so a future reopen of the
        // same path recovers the prior state.

        // Defensive: if the query was never stopped (e.g. left in a terminal Error
        // state), the FutureQueueSource may still hold a clone of the backend future
        // queue. `stop()` normally takes it already, in which case this is a no-op.
        // Take the value out first so the write-lock guard is dropped before the
        // `.await` below (never hold a lock across an await point).
        let future_queue_source = self.future_queue_source.write().await.take();
        if let Some(fq) = future_queue_source {
            fq.stop().await;
        }

        *self.checkpoint_store.write().await = None;
        *self.outbox_writer.write().await = None;
        *self.live_results_writer.write().await = None;
    }
}

pub struct QueryManager {
    instance_id: String,
    source_manager: Arc<SourceManager>,
    index_factory: Arc<crate::indexes::IndexFactory>,
    middleware_registry: Arc<MiddlewareTypeRegistry>,
    log_registry: Arc<ComponentLogRegistry>,
    /// Shared component graph — the single source of truth for component metadata,
    /// state, relationships, runtime instances, AND event history.
    graph: Arc<RwLock<ComponentGraph>>,
    /// Channel sender for routing status updates through the graph update loop.
    /// Managers send transitional states (Starting, Stopping, Reconfiguring) here;
    /// the loop applies them to the graph and records events automatically.
    update_tx: ComponentUpdateSender,
    /// Global default recovery policy. Per-query overrides this; if neither is set,
    /// defaults to `Strict`.
    default_recovery_policy: Option<crate::recovery::RecoveryPolicy>,
    /// Cached query labels extracted at registration time to avoid re-parsing
    /// queries on every `get_graph_schema()` call.
    label_cache: RwLock<HashMap<String, QueryLabels>>,
}

impl QueryManager {
    pub fn new(
        instance_id: impl Into<String>,
        source_manager: Arc<SourceManager>,
        index_factory: Arc<crate::indexes::IndexFactory>,
        middleware_registry: Arc<MiddlewareTypeRegistry>,
        log_registry: Arc<ComponentLogRegistry>,
        graph: Arc<RwLock<ComponentGraph>>,
        update_tx: ComponentUpdateSender,
        default_recovery_policy: Option<crate::recovery::RecoveryPolicy>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            source_manager,
            index_factory,
            middleware_registry,
            log_registry,
            graph,
            update_tx,
            default_recovery_policy,
            label_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Register and provision a new query from the given configuration.
    ///
    /// # Errors
    /// Returns an error if provisioning fails (e.g., invalid config or duplicate ID).
    pub async fn add_query(&self, config: QueryConfig) -> Result<()> {
        self.provision_query(config).await
    }

    pub async fn add_query_without_save(&self, config: QueryConfig) -> Result<()> {
        self.provision_query(config).await
    }

    /// Add a pre-created query instance (for testing)
    pub async fn add_query_instance_for_test(&self, query: Arc<dyn Query>) -> Result<()> {
        let query_id = query.get_config().id.clone();

        // Cache labels from the query config
        let config = query.get_config();
        match LabelExtractor::extract_labels(&config.query, &config.query_language) {
            Ok(labels) => {
                self.label_cache
                    .write()
                    .await
                    .insert(query_id.clone(), labels);
            }
            Err(e) => {
                warn!("Failed to extract labels for test query '{query_id}': {e}");
            }
        }

        let mut graph = self.graph.write().await;
        if graph.has_runtime(&query_id) {
            return Err(anyhow::anyhow!("Query with id '{query_id}' already exists"));
        }
        graph.set_runtime(&query_id, Box::new(query))?;
        Ok(())
    }

    /// Provision a query for runtime — create the DrasiQuery, initialize, and store it.
    ///
    /// This method handles runtime-only operations: creating the DrasiQuery instance,
    /// initializing it with the runtime context, and storing it in the runtime map.
    /// Graph registration (node creation, dependency edges) must be done by the caller
    /// beforehand via `ComponentGraph::register_query()`.
    pub async fn provision_query(&self, config: QueryConfig) -> Result<()> {
        // Cache labels at registration time to avoid re-parsing on every get_graph_schema() call
        match LabelExtractor::extract_labels(&config.query, &config.query_language) {
            Ok(labels) => {
                self.label_cache
                    .write()
                    .await
                    .insert(config.id.clone(), labels);
            }
            Err(e) => {
                warn!("Failed to extract labels for query '{}': {e}", config.id);
            }
        }

        // Create the query instance
        let query = DrasiQuery::new(
            &self.instance_id,
            config.clone(),
            self.source_manager.clone(),
            self.index_factory.clone(),
            self.middleware_registry.clone(),
            self.default_recovery_policy,
        )?;

        // Wire status handle to graph via context (same pattern as Source/Reaction)
        let context = crate::context::QueryRuntimeContext::new(
            &self.instance_id,
            &config.id,
            self.update_tx.clone(),
        );
        query.initialize(context).await;

        let query: Arc<dyn Query> = Arc::new(query);

        let query_id = config.id.clone();
        let should_auto_start = config.auto_start;

        // Store the runtime instance in the graph
        {
            let mut graph = self.graph.write().await;
            graph.set_runtime(&config.id, Box::new(query))?;
        }

        info!("Provisioned query: {} with bootstrap support", config.id);

        // Note: Auto-start is handled by the caller (server.add_query)
        // which has access to the data router for subscriptions
        if should_auto_start {
            info!("Query '{query_id}' is configured for auto-start (will be started by caller)");
        }

        Ok(())
    }

    /// Start a query by ID, subscribing it to its sources and beginning event processing.
    ///
    /// # Errors
    /// Returns an error if the query is not found or the start transition fails.
    pub async fn start_query(&self, id: String) -> Result<()> {
        let query =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Query>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new("query", &id))
                })?;

        crate::managers::lifecycle_helpers::start_component(&self.graph, &id, "query", &query).await
    }

    /// Stop a running query by ID, unsubscribing it from sources and halting event processing.
    ///
    /// # Errors
    /// Returns an error if the query is not found or the stop transition fails.
    pub async fn stop_query(&self, id: String) -> Result<()> {
        let query =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Query>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new("query", &id))
                })?;

        crate::managers::lifecycle_helpers::stop_component(&self.graph, &id, "query", &query).await
    }

    /// Return the current lifecycle status of the query with the given ID.
    ///
    /// # Errors
    /// Returns an error if the query is not found in the component graph.
    pub async fn get_query_status(&self, id: String) -> Result<ComponentStatus> {
        crate::managers::lifecycle_helpers::get_component_status(&self.graph, &id, "Query").await
    }

    /// Get a query instance for subscription by reactions
    /// Returns Arc<dyn Query> which reactions can use to subscribe to query results
    pub async fn get_query_instance(&self, query_id: &str) -> Result<Arc<dyn Query>, String> {
        let graph = self.graph.read().await;
        if let Some(query) = graph.get_runtime::<Arc<dyn Query>>(query_id) {
            Ok(Arc::clone(query))
        } else {
            Err(format!(
                "Query '{query_id}' not found. Available queries can be listed using list_queries()."
            ))
        }
    }

    /// Retrieve the full runtime descriptor for a query, including its status and configuration.
    ///
    /// # Errors
    /// Returns an error if the query is not found.
    pub async fn get_query(&self, id: String) -> Result<QueryRuntime> {
        let graph = self.graph.read().await;
        let query = graph.get_runtime::<Arc<dyn Query>>(&id).cloned();

        if let Some(query) = query {
            let status = graph
                .get_component(&id)
                .map(|n| n.status)
                .unwrap_or(ComponentStatus::Stopped);
            let config = query.get_config();
            let error_message = match &status {
                ComponentStatus::Error => graph.get_last_error(&id),
                _ => None,
            };
            drop(graph);
            let runtime = QueryRuntime {
                id: config.id.clone(),
                query: config.query.clone(),
                status,
                error_message,
                source_subscriptions: config.sources.clone(),
                joins: config.joins.clone(),
            };
            Ok(runtime)
        } else {
            Err(crate::managers::ComponentNotFoundError::new("query", &id).into())
        }
    }

    /// Update a query by replacing it with a new configuration.
    ///
    /// Flow: validate exists → validate status → set Reconfiguring via graph →
    /// stop if running/starting → wait for stopped → provision new →
    /// replace runtime (if still exists) → restart if was running.
    /// Graph node, edges, and event history are preserved.
    pub async fn update_query(&self, id: String, new_config: QueryConfig) -> Result<()> {
        let old_query = {
            let graph = self.graph.read().await;
            graph.get_runtime::<Arc<dyn Query>>(&id).cloned()
        };

        if let Some(old_query) = old_query {
            // Verify the new config has the same ID
            if new_config.id != id {
                return Err(anyhow::anyhow!(
                    "New query ID '{}' does not match existing query ID '{}'",
                    new_config.id,
                    id
                ));
            }
            let old_query_for_release = old_query.clone();
            crate::managers::lifecycle_helpers::reconfigure_component::<Arc<dyn Query>, _, _, _>(
                &self.graph,
                &id,
                "query",
                &old_query,
                || async {},
                || async move {
                    old_query_for_release.release_persistent_handles().await;
                    self.provision_query(new_config).await
                },
                || self.start_query(id.clone()),
            )
            .await
        } else {
            Err(crate::managers::ComponentNotFoundError::new("query", &id).into())
        }
    }

    /// Teardown a query's runtime state — stop and remove from runtime map.
    ///
    /// This method handles runtime-only operations. Graph deregistration
    /// (node removal, edge cleanup) must be done by the caller afterwards via
    /// `ComponentGraph::deregister()`.
    ///
    /// The caller should validate dependencies via `graph.can_remove()` before calling this.
    pub async fn teardown_query(&self, id: String) -> Result<()> {
        // Before teardown: grab the query config to determine if persistent
        // state cleanup is needed. After teardown_component, the runtime is
        // removed from the graph and we can no longer inspect it.
        let query_runtime = {
            let graph = self.graph.read().await;
            graph.get_runtime::<Arc<dyn Query>>(&id).cloned()
        };
        let query_config = query_runtime
            .as_ref()
            .map(|query| query.get_config().clone());

        self.label_cache.write().await.remove(&id);
        crate::managers::lifecycle_helpers::teardown_component::<Arc<dyn Query>, _, _>(
            &self.graph,
            &id,
            "query",
            ComponentType::Query,
            &self.instance_id,
            &self.log_registry,
            false,
            || async {},
        )
        .await?;
        if let Some(query) = &query_runtime {
            query.release_persistent_handles().await;
        }

        // After teardown: clear persistent indexes + checkpoints so a future
        // query with the same ID starts fresh. Only needed for persistent backends.
        // Resolve the effective backend the same way as start-up so that queries
        // relying on the instance-wide default backend are also cleaned up.
        if let Some(config) = query_config {
            if let Some(backend_ref) = config
                .storage_backend
                .as_ref()
                .or_else(|| self.index_factory.default_backend())
            {
                if !self.index_factory.is_volatile(backend_ref) {
                    info!("Query '{id}' removed — clearing persistent indexes and checkpoints");
                    match self.index_factory.build(backend_ref, &id).await {
                        Ok(created) => {
                            if let Some(checkpoint_store) = created.checkpoint_store.clone() {
                                let state = PersistentQueryState {
                                    query_id: id.clone(),
                                    element_index: Some(created.set.element_index.clone()),
                                    archive_index: Some(created.set.archive_index.clone()),
                                    result_index: Some(created.set.result_index.clone()),
                                    future_queue: Some(created.set.future_queue.clone()),
                                    checkpoint_store,
                                    session_control: created.set.session_control.clone(),
                                    outbox_writer: created.outbox_writer.clone(),
                                    live_results_writer: created.live_results_writer.clone(),
                                };
                                let reset_result = match query_runtime
                                    .as_ref()
                                    .and_then(|query| query.as_any().downcast_ref::<DrasiQuery>())
                                {
                                    Some(query) => {
                                        state
                                            .clear_recovery_state(
                                                &query.output_state,
                                                &query.publication_recovery,
                                                None,
                                                QueryRecoveryResetReason::Deprovision,
                                            )
                                            .await
                                    }
                                    None => state.clear_persistent(None, None).await,
                                };
                                if let Err(error) = reset_result {
                                    warn!(
                                        "Query '{id}' failed to clear persistent state on removal: {error:#}"
                                    );
                                }
                            } else {
                                if let Err(error) = clear_persistent_indexes(
                                    &id,
                                    &Some(created.set.element_index),
                                    &Some(created.set.archive_index),
                                    &Some(created.set.result_index),
                                    &Some(created.set.future_queue),
                                )
                                .await
                                {
                                    warn!(
                                        "Query '{id}' failed to clear persistent indexes on removal: {error:#}"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Query '{id}' failed to build indexes for cleanup on removal: {e}"
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Release persistent index-backend handles for every query runtime, without
    /// deleting any on-disk data.
    ///
    /// Intended for permanent shutdown ([`crate::DrasiLib::shutdown`]). Persistent
    /// backends such as RocksDB hold a process-exclusive lock on their data directory
    /// until every clone of their shared handle is dropped; queries retain some of
    /// those handles for their whole lifetime, so without this they are only freed
    /// when the entire `DrasiLib` is dropped. Clearing them here lets the backend
    /// release its lock while leaving persisted state intact for a future reopen.
    ///
    /// Callers should stop components first so the transient handles held by
    /// per-query tasks are already dropped by the time this runs.
    pub async fn release_all_persistent_handles(&self) {
        let query_ids: Vec<String> = self
            .list_queries()
            .await
            .into_iter()
            .map(|(id, _status)| id)
            .collect();

        for id in query_ids {
            match self.get_query_instance(&id).await {
                Ok(query) => query.release_persistent_handles().await,
                Err(e) => warn!(
                    "Failed to release persistent index handles for query '{id}' during \
                     shutdown: {e}. A persistent backend may keep its exclusive lock held."
                ),
            }
        }
    }

    /// List all registered queries with their current lifecycle status.
    pub async fn list_queries(&self) -> Vec<(String, ComponentStatus)> {
        crate::managers::lifecycle_helpers::list_components(&self.graph, &ComponentKind::Query)
            .await
    }

    pub async fn get_query_config(&self, id: &str) -> Option<QueryConfig> {
        let graph = self.graph.read().await;
        graph
            .get_runtime::<Arc<dyn Query>>(id)
            .map(|q| q.get_config().clone())
    }

    /// Return all cached query labels as (query_id, labels) pairs.
    ///
    /// Labels are extracted at query registration time and cached to avoid
    /// re-parsing the query string on every `get_graph_schema()` call.
    pub async fn get_all_query_labels(&self) -> Vec<(String, QueryLabels)> {
        self.label_cache
            .read()
            .await
            .iter()
            .map(|(id, labels)| (id.clone(), labels.clone()))
            .collect()
    }

    pub async fn get_query_results(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let query = {
            let graph = self.graph.read().await;
            graph.get_runtime::<Arc<dyn Query>>(id).cloned()
        };

        if let Some(query) = query {
            // Check if the query is running
            let status = query.status().await;
            if status != ComponentStatus::Running {
                return Err(anyhow::anyhow!("Query '{id}' is not running"));
            }

            let snapshot = query
                .fetch_snapshot()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to fetch snapshot: {e}"))?;
            Ok(snapshot.to_vec())
        } else {
            Err(crate::managers::ComponentNotFoundError::new("query", id).into())
        }
    }

    /// Start all queries that are configured for auto-start.
    ///
    /// # Errors
    /// Returns an error if any query fails to start.
    pub async fn start_all(&self) -> Result<()> {
        crate::managers::lifecycle_helpers::start_all_components::<Arc<dyn Query>, _, _>(
            &self.graph,
            &ComponentKind::Query,
            "query",
            |q| q.get_config().auto_start,
            |id, query| async move {
                // Validate and apply Starting transition atomically through the graph
                {
                    let mut graph = self.graph.write().await;
                    graph.validate_and_transition(
                        &id,
                        ComponentStatus::Starting,
                        Some("Starting query".to_string()),
                    )?;
                }

                if let Err(e) = query.start().await {
                    let mut graph = self.graph.write().await;
                    let _ = graph.validate_and_transition(
                        &id,
                        ComponentStatus::Error,
                        Some(format!("Start failed: {e}")),
                    );
                    return Err(e);
                }
                Ok(())
            },
        )
        .await
    }

    /// Stop all currently running, starting, or errored queries.
    ///
    /// # Errors
    /// Returns an error listing any queries that failed to stop.
    pub async fn stop_all(&self) -> Result<()> {
        let query_ids: Vec<String> = {
            let graph = self.graph.read().await;
            graph
                .list_by_kind(&ComponentKind::Query)
                .iter()
                .map(|(id, _)| id.clone())
                .collect()
        };

        let mut failed_queries = Vec::new();

        for id in query_ids {
            let is_active = {
                let graph = self.graph.read().await;
                graph
                    .get_component(&id)
                    .map(|n| {
                        matches!(
                            n.status,
                            ComponentStatus::Running
                                | ComponentStatus::Starting
                                | ComponentStatus::Error
                        )
                    })
                    .unwrap_or(false)
            };

            if is_active {
                if let Err(e) = self.stop_query(id.clone()).await {
                    log_component_error("Query", &id, &e.to_string());
                    failed_queries.push((id, e.to_string()));
                }
            }
        }

        if !failed_queries.is_empty() {
            let error_msg = failed_queries
                .iter()
                .map(|(id, err)| format!("{id}: {err}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!("Failed to stop some queries: {error_msg}"))
        } else {
            Ok(())
        }
    }

    /// Record a component event — delegates to the graph's centralized event history.
    pub async fn record_event(&self, event: ComponentEvent) {
        let mut graph = self.graph.write().await;
        graph.record_event(event);
    }

    /// Get events for a specific query.
    ///
    /// Returns events in chronological order (oldest first).
    pub async fn get_query_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.graph.read().await.get_events(id)
    }

    /// Get all events across all queries.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        let graph = self.graph.read().await;
        graph
            .get_all_events()
            .into_iter()
            .filter(|e| e.component_type == ComponentType::Query)
            .collect()
    }

    /// Subscribe to live logs for a query.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// Returns None if the query doesn't exist.
    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> Option<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        // Verify the query exists in the graph
        {
            let graph = self.graph.read().await;
            if !graph.has_runtime(id) {
                return None;
            }
        }

        let log_key = ComponentLogKey::new(&self.instance_id, ComponentType::Query, id);
        Some(self.log_registry.subscribe_by_key(&log_key).await)
    }

    /// Subscribe to live events for a query.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// Returns None if the query doesn't exist.
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
}

#[async_trait]
impl crate::reactions::QueryProvider for QueryManager {
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn Query>> {
        self.get_query_instance(id)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}

#[cfg(test)]
mod pipeline_characterization_tests;
