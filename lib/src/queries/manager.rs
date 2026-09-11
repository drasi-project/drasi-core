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

use drasi_core::interface::SessionError;

use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

// Import drasi-core components
use drasi_core::{
    evaluation::context::{QueryPartEvaluationContext, QueryVariables},
    evaluation::functions::FunctionRegistry,
    evaluation::variable_value::VariableValue,
    in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore,
    interface::{
        CheckpointStore, LiveResultsWriter, NoOpSessionControl, OutboxWriter, SessionControl,
        SessionGuard,
    },
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

/// Convert QueryVariables (`BTreeMap<Box<str>, VariableValue>`) to JSON
fn convert_query_variables_to_json(vars: &QueryVariables) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    for (key, value) in vars.iter() {
        result.insert(key.to_string(), convert_variable_value_to_json(value));
    }
    serde_json::Value::Object(result)
}

/// Convert a single VariableValue to JSON
fn convert_variable_value_to_json(value: &VariableValue) -> serde_json::Value {
    match value {
        VariableValue::Null => serde_json::Value::Null,
        VariableValue::Bool(b) => serde_json::Value::Bool(*b),
        VariableValue::Float(f) => {
            if f.is_f64() {
                // from_f64 returns None for NaN/Infinity, but is_f64() already checks finiteness
                let s = f.to_string();
                s.parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or_else(|| serde_json::Value::String(s))
            } else {
                serde_json::Value::String(f.to_string())
            }
        }
        VariableValue::Integer(i) => {
            if let Some(val) = i.as_i64() {
                serde_json::Value::Number(serde_json::Number::from(val))
            } else if let Some(val) = i.as_u64() {
                serde_json::Value::Number(serde_json::Number::from(val))
            } else {
                serde_json::Value::String(i.to_string())
            }
        }
        VariableValue::String(s) => serde_json::Value::String(s.clone()),
        VariableValue::List(list) => {
            serde_json::Value::Array(list.iter().map(convert_variable_value_to_json).collect())
        }
        VariableValue::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map.iter() {
                result.insert(k.clone(), convert_variable_value_to_json(v));
            }
            serde_json::Value::Object(result)
        }
        VariableValue::Date(d) => serde_json::Value::String(d.to_string()),
        VariableValue::LocalTime(t) => serde_json::Value::String(t.to_string()),
        VariableValue::ZonedTime(t) => serde_json::Value::String(t.to_string()),
        // Query/reaction output uses plain strings for temporal values.
        // The tagged datetime envelope in ElementValue JSON is internal-only.
        VariableValue::LocalDateTime(dt) => serde_json::Value::String(dt.to_string()),
        VariableValue::ZonedDateTime(dt) => serde_json::Value::String(dt.datetime().to_rfc3339()),
        VariableValue::Duration(d) => serde_json::Value::String(d.to_string()),
        // For complex types, convert to string representation
        _ => serde_json::Value::String(format!("{value:?}")),
    }
}

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

/// Bootstrap phase tracking for each source
#[derive(Debug, Clone, PartialEq)]
enum BootstrapPhase {
    NotStarted,
    InProgress,
    Completed,
}

fn convert_query_results(results: &[QueryPartEvaluationContext]) -> Vec<ResultDiff> {
    results
        .iter()
        .filter_map(|ctx| match ctx {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            } => Some(ResultDiff::Add {
                data: convert_query_variables_to_json(after),
                row_signature: *row_signature,
            }),
            QueryPartEvaluationContext::Removing {
                before,
                row_signature,
            } => Some(ResultDiff::Delete {
                data: convert_query_variables_to_json(before),
                row_signature: *row_signature,
            }),
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } => {
                let after_json = convert_query_variables_to_json(after);
                Some(ResultDiff::Update {
                    data: after_json.clone(),
                    before: convert_query_variables_to_json(before),
                    after: after_json,
                    grouping_keys: None,
                    row_signature: *row_signature,
                })
            }
            // NOTE: When a group empties (last contributor removed), core emits
            // Aggregation { default_after: true, .. } with identity values (count:0,
            // sum:0, etc.) rather than Removing. Proper empty-group → Delete detection
            // requires core-level `is_at_identity()` on each accumulator (see PR #409).
            // Without that infrastructure, this conversion preserves current behavior:
            // the row stays in the result set with zeroed-out values.
            QueryPartEvaluationContext::Aggregation {
                before,
                after,
                row_signature,
                ..
            } => Some(ResultDiff::Aggregation {
                before: before.as_ref().map(convert_query_variables_to_json),
                after: convert_query_variables_to_json(after),
                row_signature: *row_signature,
            }),
            QueryPartEvaluationContext::Noop => None,
        })
        .collect()
}

async fn acquire_query_root(
    control: Arc<dyn SessionControl>,
) -> Result<SessionGuard, drasi_core::interface::IndexError> {
    loop {
        match SessionGuard::begin(control.clone()).await {
            Err(error) if error.session_error() == Some(SessionError::SessionBusy) => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            result => return result,
        }
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
    // Track subscription tasks for cleanup
    subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    // Abort handles for bootstrap + supervisor tasks (for cleanup on stop)
    bootstrap_abort_handles: Arc<RwLock<Vec<tokio::task::AbortHandle>>>,
    // Track bootstrap state per source
    bootstrap_state: Arc<RwLock<HashMap<String, BootstrapPhase>>>,
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
    // Keep source watermarks pinned after processor failure until explicit unsubscribe.
    subscribed_sources: RwLock<HashMap<String, Option<Arc<std::sync::atomic::AtomicU64>>>>,
    // Per-query output metrics (outbox, sequence, snapshot health)
    output_metrics: Arc<QueryOutputMetrics>,
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
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            bootstrap_abort_handles: Arc::new(RwLock::new(Vec::new())),
            bootstrap_state: Arc::new(RwLock::new(HashMap::new())),
            index_factory,
            middleware_registry,
            future_queue_source: Arc::new(RwLock::new(None)),
            checkpoint_store: Arc::new(RwLock::new(None)),
            outbox_writer: Arc::new(RwLock::new(None)),
            live_results_writer: Arc::new(RwLock::new(None)),
            bootstrap_timeout,
            resolved_recovery_policy,
            subscribed_sources: RwLock::new(HashMap::new()),
            output_metrics: Arc::new(QueryOutputMetrics::new()),
        })
    }

    /// Initialize the query with runtime context.
    ///
    /// Wires the status handle to the component graph, following the same
    /// pattern as Source and Reaction initialization.
    pub async fn initialize(&self, context: crate::context::QueryRuntimeContext) {
        self.base.initialize(context).await;
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
    async fn quiesce(&self) -> Result<()> {
        {
            let mut handles = self.bootstrap_abort_handles.write().await;
            for handle in handles.iter() {
                handle.abort();
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while handles.iter().any(|handle| !handle.is_finished()) {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .context("Bootstrap tasks did not stop after cancellation")?;
            handles.clear();
        }
        let subscriptions: Vec<_> = self.subscription_tasks.write().await.drain(..).collect();
        for handle in &subscriptions {
            handle.abort();
        }
        for handle in subscriptions {
            if let Err(error) = handle.await {
                if !error.is_cancelled() {
                    error!(
                        "Query '{}' subscription task failed: {error}",
                        self.base.config.id
                    );
                }
            }
        }
        self.base.stop_processing().await;
        let future_source = self.future_queue_source.write().await.take();
        if let Some(source) = future_source {
            source.stop().await;
        }
        let subscriptions: Vec<_> = self.subscribed_sources.write().await.drain().collect();
        for (source_id, _position_handle) in subscriptions {
            if let Some(source) = self.source_manager.get_source_instance(&source_id).await {
                source.remove_position_handle(&self.base.config.id).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl DrasiQuery {
    /// Count active subscription forwarder tasks (testing helper)
    pub async fn subscription_task_count(&self) -> usize {
        self.subscription_tasks.read().await.len()
    }

    /// Access the checkpoint store (for internal/test use only).
    #[doc(hidden)]
    pub async fn get_checkpoint_store(&self) -> Option<Arc<dyn CheckpointStore>> {
        self.checkpoint_store.read().await.clone()
    }
}

async fn persist_query_config_hash(
    session_control: Arc<dyn SessionControl>,
    checkpoint_store: &Arc<dyn CheckpointStore>,
    hash: u64,
) -> Result<()> {
    let guard = SessionGuard::begin(session_control)
        .await
        .context("Failed to begin session for query configuration")?;
    guard
        .mark_dirty()
        .context("Query configuration root is not active")?;
    checkpoint_store
        .write_config_hash(hash)
        .await
        .context("Failed to store the query configuration")?;
    guard
        .commit()
        .await
        .context("Failed to commit the query configuration")?;
    Ok(())
}

async fn rebuild_persistent_query_store(
    query_id: &str,
    element_index: &Option<Arc<dyn drasi_core::interface::ElementIndex>>,
    archive_index: &Option<Arc<dyn drasi_core::interface::ElementArchiveIndex>>,
    result_index: &Option<Arc<dyn drasi_core::interface::ResultIndex>>,
    future_queue: &Option<Arc<dyn drasi_core::interface::FutureQueue>>,
    checkpoint_store: &Arc<dyn CheckpointStore>,
    outbox_writer: Option<&Arc<dyn OutboxWriter>>,
    live_results_writer: Option<&Arc<dyn LiveResultsWriter>>,
    session_control: Arc<dyn SessionControl>,
) -> Result<()> {
    let guard = SessionGuard::begin(session_control.clone())
        .await
        .context("Failed to begin session for clearing persistent query state")?;
    guard
        .mark_dirty()
        .context("Query rebuild root is not active")?;
    clear_persistent_indexes(
        query_id,
        element_index,
        archive_index,
        result_index,
        future_queue,
    )
    .await?;
    if let Some(writer) = outbox_writer {
        writer
            .clear(query_id)
            .await
            .with_context(|| format!("Query '{query_id}' failed to clear outbox"))?;
    }
    if let Some(writer) = live_results_writer {
        writer
            .clear(query_id)
            .await
            .with_context(|| format!("Query '{query_id}' failed to clear live results"))?;
    }
    checkpoint_store
        .clear_checkpoints()
        .await
        .with_context(|| format!("Query '{query_id}' failed to clear checkpoints"))?;
    checkpoint_store
        .write_result_sequence(query_id, 0)
        .await
        .with_context(|| format!("Query '{query_id}' failed to reset result sequence"))?;
    guard
        .commit()
        .await
        .context("Failed to commit persistent query rebuild")?;
    Ok(())
}

async fn clear_persistent_indexes(
    query_id: &str,
    element_index: &Option<Arc<dyn drasi_core::interface::ElementIndex>>,
    archive_index: &Option<Arc<dyn drasi_core::interface::ElementArchiveIndex>>,
    result_index: &Option<Arc<dyn drasi_core::interface::ResultIndex>>,
    future_queue: &Option<Arc<dyn drasi_core::interface::FutureQueue>>,
) -> anyhow::Result<()> {
    use drasi_core::interface::IndexError;

    if let Some(ei) = element_index {
        ei.clear()
            .await
            .with_context(|| format!("Query '{query_id}' failed to clear element index"))?;
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
        ri.clear()
            .await
            .with_context(|| format!("Query '{query_id}' failed to clear result index"))?;
    }
    if let Some(fq) = future_queue {
        fq.clear()
            .await
            .with_context(|| format!("Query '{query_id}' failed to clear future queue"))?;
    }
    Ok(())
}

#[async_trait]
impl Query for DrasiQuery {
    async fn start(&self) -> Result<()> {
        log_component_start("Query", &self.base.config.id);

        self.bootstrap_state.write().await.clear();

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
        if let Err(error) = self.quiesce().await {
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!(
                        "Previous query generation could not stop: {error:#}"
                    )),
                )
                .await;
            return Err(error);
        }
        while self.priority_queue.try_dequeue().await.is_some() {}
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
        let session_control: Arc<dyn SessionControl>;

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

            // Use backend-provided checkpoint store, or create in-memory fallback
            checkpoint_store = match created.checkpoint_store {
                Some(store) => store,
                None => {
                    // Backend didn't provide one; reuse persisted or create new
                    let existing = self.checkpoint_store.read().await.clone();
                    existing.unwrap_or_else(|| Arc::new(InMemoryCheckpointStore::new()))
                }
            };

            // Store persistent writers if provided by the backend
            *self.outbox_writer.write().await = created.outbox_writer;
            *self.live_results_writer.write().await = created.live_results_writer;
            // Hold references for potential clearing before passing to builder
            element_index = Some(created.set.element_index.clone());
            archive_index = Some(created.set.archive_index.clone());
            result_index = Some(created.set.result_index.clone());
            future_queue = Some(created.set.future_queue.clone());
            session_control = created.set.session_control.clone();

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
            session_control = Arc::new(NoOpSessionControl);
            builder = builder.with_session_control(session_control.clone());
        };

        // Persist the checkpoint_store for future stop/start cycles
        *self.checkpoint_store.write().await = Some(checkpoint_store.clone());

        // Extract labels from the query for bootstrap
        let labels = match crate::queries::LabelExtractor::extract_labels(
            &query_str,
            &self.base.config.query_language,
        ) {
            Ok(labels) => labels,
            Err(e) => {
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Failed to extract query labels: {e}")),
                    )
                    .await;
                return Err(e.context("Failed to extract query labels"));
            }
        };

        let subscription_plan =
            match crate::queries::SubscriptionSettingsBuilder::build_subscriptions(
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

        // Read the last checkpoints and propagate source_position to subscription settings
        // so sources can resume from where they left off.
        //
        // Only propagate checkpoint recovery when the checkpoint store is persistent.
        // Volatile (in-memory) stores don't survive restarts, and their paired element
        // indexes rebuild fresh on each start — bootstrap must run to populate the
        // graph state. Skipping bootstrap against an empty graph would produce
        // incorrect results.
        let source_selections = Arc::new(
            subscription_plan
                .selections
                .into_iter()
                .map(|(source_id, selection)| (source_id, Arc::new(selection)))
                .collect::<HashMap<_, _>>(),
        );
        let mut subscription_settings = subscription_plan.settings;
        let has_persistent_backend = checkpoint_store.is_persistent();
        let query_config_hash = super::compute_config_hash(&self.base.config);
        let mut needs_config_hash_write = false;
        let mut checkpoint_sequences_per_source: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        if has_persistent_backend {
            let startup: Result<_> = async {
                let guard = SessionGuard::begin(session_control.clone())
                    .await
                    .context("Failed to begin session for config hash check")?;
                let stored_hash = checkpoint_store
                    .read_config_hash()
                    .await
                    .context("Failed to read the stored query configuration")?;
                let checkpoints = if stored_hash == Some(query_config_hash) {
                    checkpoint_store
                        .read_all_checkpoints()
                        .await
                        .context("Failed to read source checkpoints")?
                } else {
                    HashMap::new()
                };
                guard
                    .commit()
                    .await
                    .context("Failed to commit query startup")?;
                if stored_hash != Some(query_config_hash) {
                    let outbox = self.outbox_writer.read().await.clone();
                    let live = self.live_results_writer.read().await.clone();
                    rebuild_persistent_query_store(
                        &self.base.config.id,
                        &element_index,
                        &archive_index,
                        &result_index,
                        &future_queue,
                        &checkpoint_store,
                        outbox.as_ref(),
                        live.as_ref(),
                        session_control.clone(),
                    )
                    .await
                    .context("Failed to rebuild persistent query state")?;
                    {
                        let mut state = self.output_state.write().await;
                        let capacity = state.outbox_capacity();
                        *state = QueryOutputState::new(capacity);
                    }
                }
                Ok((checkpoints, stored_hash != Some(query_config_hash)))
            }
            .await;
            let checkpoints = match startup {
                Ok((checkpoints, rebuilt)) => {
                    needs_config_hash_write = rebuilt;
                    checkpoints
                }
                Err(error) => {
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Query startup failed: {error:#}")),
                        )
                        .await;
                    return Err(error);
                }
            };
            for settings in &mut subscription_settings {
                settings.request_position_handle = true;
                if let Some(checkpoint) = checkpoints.get(&settings.source_id) {
                    checkpoint_sequences_per_source
                        .insert(settings.source_id.clone(), checkpoint.sequence);
                    settings.resume_sequence = Some(checkpoint.sequence);
                    settings.resume_from = checkpoint.source_position.clone();
                }
            }
        }

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

        let mut bootstrap_channels: Vec<(
            String,
            tokio::sync::mpsc::Receiver<crate::channels::BootstrapEvent>,
            Option<
                tokio::sync::oneshot::Receiver<anyhow::Result<crate::bootstrap::BootstrapResult>>,
            >,
        )> = Vec::new();
        let mut subscription_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

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
                    for handle in subscription_tasks.drain(..) {
                        handle.abort();
                        let _ = handle.await;
                    }
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
                subscription_tasks.clear();
                position_handles.clear();
                self.bootstrap_state.write().await.clear();
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
                                            for handle in subscription_tasks.drain(..) {
                                                handle.abort();
                                                let _ = handle.await;
                                            }
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
                                                for handle in subscription_tasks.drain(..) {
                                                    handle.abort();
                                                    let _ = handle.await;
                                                }
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
                                            for handle in subscription_tasks.drain(..) {
                                                handle.abort();
                                                let _ = handle.await;
                                            }

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

                                            if has_persistent_backend {
                                                let outbox =
                                                    self.outbox_writer.read().await.clone();
                                                let live =
                                                    self.live_results_writer.read().await.clone();
                                                if let Err(error) = rebuild_persistent_query_store(
                                                    &self.base.config.id,
                                                    &element_index,
                                                    &archive_index,
                                                    &result_index,
                                                    &future_queue,
                                                    &checkpoint_store,
                                                    outbox.as_ref(),
                                                    live.as_ref(),
                                                    session_control.clone(),
                                                )
                                                .await
                                                {
                                                    let msg = format!(
                                                        "Query '{}' auto-reset failed: {error:#}",
                                                        self.base.config.id
                                                    );
                                                    error!("{msg}");
                                                    self.base
                                                        .set_status(
                                                            ComponentStatus::Error,
                                                            Some(msg),
                                                        )
                                                        .await;
                                                    return Err(error.context(
                                                        "AutoReset aborted: failed to clear persistent state",
                                                    ));
                                                }
                                                {
                                                    let mut state = self.output_state.write().await;
                                                    let capacity = state.outbox_capacity();
                                                    *state = QueryOutputState::new(capacity);
                                                }
                                                needs_config_hash_write = true;
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
                        for handle in subscription_tasks.drain(..) {
                            handle.abort();
                            let _ = handle.await;
                        }
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
                // Also initialize bootstrap state only for sources that support bootstrap
                if let Some(bootstrap_rx) = subscription_response.bootstrap_receiver {
                    bootstrap_channels.push((
                        source_id.clone(),
                        bootstrap_rx,
                        subscription_response.bootstrap_result_receiver,
                    ));
                    self.bootstrap_state
                        .write()
                        .await
                        .insert(source_id.to_string(), BootstrapPhase::NotStarted);
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

                subscription_tasks.push(task);
            }

            // All sources subscribed successfully — break out of the retry loop
            break;
        }

        // Store subscription tasks and record subscribed source IDs for cleanup in stop()
        *self.subscription_tasks.write().await = subscription_tasks;
        *self.subscribed_sources.write().await = sources_to_subscribe
            .iter()
            .map(|(id, _, _)| (id.clone(), position_handles.get(id).cloned()))
            .collect();

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

        debug!(
            "Query '{}' setting up FutureQueueSource for temporal queries",
            self.base.config.id
        );

        let future_queue_source = Arc::new(FutureQueueSource::new(
            continuous_query.future_queue(),
            self.base.config.id.clone(),
        ));

        let fq_receiver = future_queue_source
            .subscribe()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe to FutureQueueSource: {e}"))?;

        *self.future_queue_source.write().await = Some(Arc::clone(&future_queue_source));

        let continuous_query = Arc::new(continuous_query);

        let output_delivery = Arc::new(super::output_delivery::OutputDelivery {
            query_id: self.base.config.id.clone(),
            state: self.output_state.clone(),
            dispatchers: self.base.dispatchers.clone(),
            outbox_writer: self.outbox_writer.read().await.clone(),
            live_results_writer: self.live_results_writer.read().await.clone(),
            checkpoint_store: checkpoint_store.clone(),
            metrics: self.output_metrics.clone(),
        });
        if has_persistent_backend {
            let restoration: Result<()> = async {
                let root = SessionGuard::begin(session_control.clone()).await?;
                output_delivery.restore().await?;
                root.commit().await?;
                Ok(())
            }
            .await;
            if let Err(error) = restoration {
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Failed to restore query output: {error:#}")),
                    )
                    .await;
                return Err(error);
            }
        }

        // Gate that blocks the streaming event processor until bootstrap completes.
        // Events buffer safely in the priority queue during bootstrap.
        let bootstrap_gate = Arc::new(Notify::new());

        // NEW: Handle bootstrap channels
        if !bootstrap_channels.is_empty() {
            info!(
                "Query '{}' starting bootstrap from {} sources",
                self.base.config.id,
                bootstrap_channels.len()
            );

            // Emit bootstrapStarted control signal
            let mut metadata = HashMap::new();
            metadata.insert(
                "control_signal".to_string(),
                serde_json::json!("bootstrapStarted"),
            );
            metadata.insert(
                "source_count".to_string(),
                serde_json::json!(bootstrap_channels.len()),
            );

            let control_result = QueryResult::new(
                self.base.config.id.clone(),
                0,
                chrono::Utc::now(),
                vec![],
                metadata,
            );

            // Dispatch the control signal to all subscribed reactions
            self.base.dispatch_query_result(control_result).await.ok();
            info!(
                "[BOOTSTRAP] Emitted bootstrapStarted signal for query '{}'",
                self.base.config.id
            );

            // Process bootstrap events from each source
            let continuous_query_clone = continuous_query.clone();
            let base_dispatchers = self.base.dispatchers.clone();
            let query_id = self.base.config.id.clone();
            let bootstrap_state = self.bootstrap_state.clone();
            let instance_id = self.instance_id.clone();
            let bootstrap_processing = Arc::new(tokio::sync::Mutex::new(()));

            let mut bootstrap_handles = Vec::new();
            let mut abort_handles = Vec::new();

            for (source_id, mut bootstrap_rx, bootstrap_result_rx) in bootstrap_channels {
                // Mark source bootstrap as in progress
                bootstrap_state
                    .write()
                    .await
                    .insert(source_id.to_string(), BootstrapPhase::InProgress);

                info!(
                    "[BOOTSTRAP] Query '{query_id}' processing bootstrap from source '{source_id}'"
                );

                let continuous_query_ref = continuous_query_clone.clone();
                let query_id_clone = query_id.clone();
                let source_id_clone = source_id.clone();
                let bootstrap_state_clone = bootstrap_state.clone();
                let instance_id_clone = instance_id.clone();
                let output_delivery = output_delivery.clone();
                let session_control = session_control.clone();
                let bootstrap_processing = bootstrap_processing.clone();
                let source_selection =
                    source_selections
                        .get(&source_id)
                        .cloned()
                        .with_context(|| {
                            format!("Missing selection for bootstrap source '{source_id}'")
                        })?;

                let span = tracing::info_span!(
                    "query_bootstrap",
                    instance_id = %instance_id_clone,
                    component_id = %query_id,
                    component_type = "query"
                );
                let handle: tokio::task::JoinHandle<(String, anyhow::Result<Option<crate::bootstrap::BootstrapResult>>)> = tokio::spawn(
                    async move {
                        let mut count = 0u64;

                        while let Some(bootstrap_event) = bootstrap_rx.recv().await {
                            count += 1;

                            let processed: Result<()> = async {
                                let _processing = bootstrap_processing.lock().await;
                                let mut root = acquire_query_root(session_control.clone()).await?;
                                let mut diffs = Vec::new();
                                let provisional = continuous_query_ref
                                    .process_source_change_in_with_hook(
                                        &mut root,
                                        drasi_core::models::SourceInput::with_normalizer(
                                            bootstrap_event.change,
                                            source_selection.clone(),
                                        ),
                                        |results| {
                                            diffs = convert_query_results(results);
                                            async { Ok(()) }
                                        },
                                    ).await?;
                                let staged = output_delivery.stage_bootstrap(diffs, &root).await?;
                                let receipt = root.commit_with_receipt().await?;
                                provisional.into_committed(&receipt)?;
                                output_delivery.publish_bootstrap(staged, &receipt).await?;
                                Ok(())
                            }.await;
                            if let Err(error) = processed {
                                error!(
                                    "[BOOTSTRAP] Query '{query_id_clone}' failed to process bootstrap event from source '{source_id_clone}': {error:#}"
                                );
                                return (
                                    source_id_clone,
                                    Err(error.context("Bootstrap query evaluation failed")),
                                );
                            }
                        }

                        info!(
                            "[BOOTSTRAP] Query '{query_id_clone}' completed bootstrap from source '{source_id_clone}' ({count} events)"
                        );

                        // Mark source bootstrap as completed
                        {
                            let mut state = bootstrap_state_clone.write().await;
                            state.insert(source_id_clone.to_string(), BootstrapPhase::Completed);
                        }

                        // Await the BootstrapResult from the source's bootstrap provider.
                        // This carries the optional source_position snapshot boundary.
                        // A provider failure (Ok(Err)) or a dropped result channel
                        // (Err) is propagated as an Err so the supervisor can
                        // transition the query to the Error state. Errors are carried
                        // as anyhow::Error to preserve the context chain, matching the
                        // internal-module convention.
                        let bootstrap_result: anyhow::Result<Option<crate::bootstrap::BootstrapResult>> =
                            if let Some(rx) = bootstrap_result_rx {
                                match rx.await {
                                    Ok(Ok(result)) => {
                                        debug!(
                                            "[BOOTSTRAP] Query '{}' received handover from source '{}': \
                                             source_position={:?}",
                                            query_id_clone, source_id_clone,
                                            result.source_position.as_ref().map(|p| p.len())
                                        );
                                        Ok(Some(result))
                                    }
                                    Ok(Err(e)) => {
                                        error!(
                                            "[BOOTSTRAP] Query '{query_id_clone}' bootstrap provider failed for source '{source_id_clone}': {e:#}"
                                        );
                                        Err(e).context(format!("source '{source_id_clone}'"))
                                    }
                                    Err(_) => {
                                        // The sender was dropped without producing a
                                        // result. This is a silent-failure path (e.g. a
                                        // provider task panicked before sending), so
                                        // treat it as a bootstrap failure rather than
                                        // letting the query proceed to Running.
                                        error!(
                                            "[BOOTSTRAP] Query '{query_id_clone}' bootstrap result channel dropped for source '{source_id_clone}' (provider may have failed)"
                                        );
                                        Err(anyhow::anyhow!(
                                            "source '{source_id_clone}': bootstrap result channel dropped without a result"
                                        ))
                                    }
                                }
                            } else {
                                Ok(None)
                            };

                        (source_id_clone, bootstrap_result)
                    }
                    .instrument(span),
                );
                abort_handles.push(handle.abort_handle());
                bootstrap_handles.push(handle);
            }

            // Supervisor task: joins all bootstrap tasks, computes handover
            // checkpoints, emits the bootstrapCompleted signal, and opens the gate.
            // Also handles panics by transitioning to Error.
            {
                let bootstrap_gate_clone = bootstrap_gate.clone();
                let reporter_clone = self.base.status_handle();
                let query_id_clone = self.base.config.id.clone();
                let instance_id_clone = self.instance_id.clone();
                let base_dispatchers_clone = base_dispatchers.clone();
                let checkpoint_store_for_supervisor = checkpoint_store.clone();
                let session_control_for_supervisor = session_control.clone();
                let needs_config_hash_write = needs_config_hash_write;
                let query_config_hash = query_config_hash;

                let span = tracing::info_span!(
                    "bootstrap_supervisor",
                    instance_id = %instance_id_clone,
                    component_id = %query_id_clone,
                    component_type = "query"
                );
                let supervisor_handle = tokio::spawn(
                    async move {
                        let join_results = futures::future::join_all(bootstrap_handles).await;
                        let panic_count = join_results.iter().filter(|r| matches!(r, Err(e) if e.is_panic())).count();

                        // Collect bootstrap provider failures reported by the per-source
                        // tasks. Each error already carries the source id via anyhow
                        // context; render the full chain single-line with `{:#}`.
                        let failures: Vec<String> = join_results
                            .iter()
                            .filter_map(|r| r.as_ref().ok())
                            .filter_map(|(_, result)| {
                                result.as_ref().err().map(|e| format!("{e:#}"))
                            })
                            .collect();

                        if panic_count > 0 || !failures.is_empty() {
                            let mut details = Vec::new();
                            if panic_count > 0 {
                                details.push(format!("{panic_count} task(s) panicked"));
                            }
                            details.extend(failures.iter().cloned());
                            let detail = details.join("; ");

                            error!(
                                "[BOOTSTRAP] Query '{query_id_clone}' bootstrap failed ({detail}), \
                                 transitioning to Error and opening gate"
                            );

                            // The same failure reason is reported in the status so callers
                            // of get_query_status() can see why bootstrap failed without
                            // having to correlate against logs. This is an embedded,
                            // in-process API and the identical text is already emitted to
                            // the operator log above, so the status carries no information
                            // not already present there.
                            reporter_clone.set_status(
                                ComponentStatus::Error,
                                Some(format!("Bootstrap failed: {detail}")),
                            ).await;

                            // Open the gate so the processor doesn't block
                            bootstrap_gate_clone.notify_one();
                            return;
                        }

                        // Persist the bootstrap snapshot boundary (source_position)
                        // as a recovery checkpoint so a crash after bootstrap but
                        // before the first streaming event doesn't lose progress and
                        // avoids a redundant re-bootstrap. Bootstrap events don't go
                        // through dispatch_event(), so there is no sequence yet; use
                        // 0 as the sentinel sequence alongside the source_position.
                        let mut handover_positions: std::collections::HashMap<String, Option<bytes::Bytes>> =
                            std::collections::HashMap::new();

                        for (source_id, bootstrap_result) in join_results.iter().filter_map(|r| r.as_ref().ok()) {
                            if let Ok(Some(br)) = bootstrap_result {
                                if let Some(pos) = &br.source_position {
                                    // Validate source_position size (same limit as dispatch_event)
                                    if pos.len() > crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES {
                                        warn!(
                                            "[BOOTSTRAP] Query '{query_id_clone}' source '{source_id}' \
                                             bootstrap source_position is {} bytes (> {} limit); \
                                             dropping position, no recovery checkpoint persisted",
                                            pos.len(),
                                            crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES
                                        );
                                    } else {
                                        handover_positions.insert(source_id.clone(), Some(pos.clone()));
                                    }
                                }
                            }
                        }

                        info!(
                            "[BOOTSTRAP] Query '{query_id_clone}' all sources completed bootstrap, \
                             {} recovery checkpoint(s) to persist",
                            handover_positions.len()
                        );

                        // Persist recovery checkpoints before opening the gate.
                        // This ensures crash-after-bootstrap-before-first-streaming-event
                        // doesn't lose progress and avoids a redundant re-bootstrap.
                        if !handover_positions.is_empty() || needs_config_hash_write {
                            let handover: Result<()> = async {
                                let guard = SessionGuard::begin(session_control_for_supervisor.clone())
                                    .await
                                    .context("Failed to begin bootstrap checkpoint transaction")?;
                                guard.mark_dirty().context("Bootstrap checkpoint root is not active")?;
                                for (source_id, position) in &handover_positions {
                                    checkpoint_store_for_supervisor
                                        .stage_checkpoint(source_id, 0, position.as_ref())
                                        .await
                                        .with_context(|| format!("Failed to stage bootstrap checkpoint for '{source_id}'"))?;
                                }
                                if needs_config_hash_write {
                                    checkpoint_store_for_supervisor
                                        .write_config_hash(query_config_hash)
                                        .await
                                        .context("Failed to store the query configuration")?;
                                }
                                guard.commit().await.context("Failed to commit bootstrap checkpoints")?;
                                Ok(())
                            }.await;
                            if let Err(error) = handover {
                                reporter_clone.set_status(
                                    ComponentStatus::Error,
                                    Some(format!("Bootstrap checkpoint persistence failed: {error:#}")),
                                ).await;
                                bootstrap_gate_clone.notify_one();
                                return;
                            }
                        }

                        // Emit bootstrapCompleted control signal
                        let mut metadata = HashMap::new();
                        metadata.insert(
                            "control_signal".to_string(),
                            serde_json::json!("bootstrapCompleted"),
                        );

                        let control_result = QueryResult::new(
                            query_id_clone.clone(),
                            0,
                            chrono::Utc::now(),
                            vec![],
                            metadata,
                        );

                        let arc_result = Arc::new(control_result);

                        // Dispatch bootstrapCompleted signal to all reactions
                        let dispatchers = base_dispatchers_clone.read().await;
                        let mut dispatched = false;
                        for dispatcher in dispatchers.iter() {
                            if dispatcher.dispatch_change(arc_result.clone()).await.is_ok() {
                                dispatched = true;
                            }
                        }

                        if !dispatched {
                            debug!(
                                "No reactions subscribed to query '{query_id_clone}' for bootstrapCompleted signal"
                            );
                        } else {
                            info!(
                                "[BOOTSTRAP] Emitted bootstrapCompleted signal for query '{query_id_clone}'"
                            );
                        }

                        // Open the bootstrap gate so the event processor can start
                        bootstrap_gate_clone.notify_one();
                        info!("[BOOTSTRAP] Query '{query_id_clone}' bootstrap gate opened");
                    }
                    .instrument(span),
                );
                abort_handles.push(supervisor_handle.abort_handle());
            }

            // Store abort handles for cleanup on stop()
            *self.bootstrap_abort_handles.write().await = abort_handles;
        } else {
            info!(
                "Query '{}' no bootstrap channels, skipping bootstrap",
                self.base.config.id
            );
            if needs_config_hash_write {
                if let Err(error) = persist_query_config_hash(
                    session_control.clone(),
                    &checkpoint_store,
                    query_config_hash,
                )
                .await
                {
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Failed to store query configuration: {error:#}")),
                        )
                        .await;
                    return Err(error);
                }
            }
            bootstrap_gate.notify_one();
        }

        // Spawn FutureQueueSource forwarder task (same pattern as other sources)
        {
            let fq_priority_queue = self.priority_queue.clone();
            let fq_forwarder = tokio::spawn(async move {
                let mut receiver = fq_receiver;
                while let Ok(event) = receiver.recv().await {
                    fq_priority_queue.enqueue_wait(event).await;
                }
            });
            self.subscription_tasks.write().await.push(fq_forwarder);
        }

        // Spawn event processor task that reads from priority queue
        let continuous_query_for_processor = continuous_query.clone();
        let checkpoint_store_for_processor = checkpoint_store.clone();
        let session_control_for_processor = session_control.clone();
        let query_id = self.base.config.id.clone();
        let task_handle_clone = self.base.task_handle.clone();
        let priority_queue = self.priority_queue.clone();
        let instance_id = self.instance_id.clone();
        let reporter_for_processor = self.base.status_handle();
        let fq_source_for_processor = Arc::clone(&future_queue_source);
        let mut future_errors = future_queue_source.subscribe_errors();
        let position_handles_for_processor = position_handles;
        // Create shutdown channel for graceful termination
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let span = tracing::info_span!(
            "query_processor",
            instance_id = %instance_id,
            component_id = %query_id,
            component_type = "query"
        );
        let handle = tokio::spawn(
            async move {
                info!("Query '{query_id}' waiting for bootstrap gate before processing events");

                // Wait for bootstrap to complete (or immediate signal if no bootstrap).
                // If shutdown arrives while waiting, exit cleanly.
                tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        info!(
                            "Query '{query_id}' received shutdown during bootstrap wait, exiting"
                        );
                        return;
                    }

                    _ = bootstrap_gate.notified() => {
                        info!("Query '{query_id}' bootstrap gate opened, starting event processing");
                    }
                }

                // Bootstrap complete — transition to Running only if still Starting.
                // If stop() was called during bootstrap, status may already be
                // Stopping and we must not overwrite it.
                let should_run = matches!(reporter_for_processor.get_status().await, ComponentStatus::Starting);

                if should_run {
                    reporter_for_processor.set_status(
                        ComponentStatus::Running,
                        Some("Query started successfully".to_string()),
                    ).await;
                } else {
                    let current = reporter_for_processor.get_status().await;
                    warn!(
                        "Query '{query_id}' bootstrap completed but status is {current:?}, \
                         skipping transition to Running"
                    );
                    if current != ComponentStatus::Running {
                        return;
                    }
                }

                // Start FutureQueueSource after bootstrap completes
                if let Err(e) = fq_source_for_processor.start().await {
                    error!("Query '{query_id}' failed to start FutureQueueSource: {e}");
                    reporter_for_processor
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Future queue start failed: {e}")),
                        )
                        .await;
                    return;
                }

                info!("Query '{query_id}' starting priority queue event processor");

                // Initialize the crash-recovery dedup filter from stored checkpoints
                // (if resuming) so buffered streaming events at or below the
                // checkpoint sequence are filtered on replay.
                let mut dedup = super::SequenceDedup::new(checkpoint_sequences_per_source.clone());

                'events: loop {
                    // Check if query is still running
                    let current_status = reporter_for_processor.get_status().await;
                    if !matches!(current_status, ComponentStatus::Running) {
                        info!(
                            "Query '{query_id}' status changed to non-running ({current_status:?}), exiting processing loop"
                        );
                        break;
                    }

                    tokio::select! {
                        biased;

                        _ = &mut shutdown_rx => {
                            info!(
                                "Query '{query_id}' received shutdown signal, exiting processing loop"
                            );
                            break;
                        }

                        changed = future_errors.changed() => {
                            let message = match changed {
                                Ok(()) => future_errors.borrow_and_update().clone(),
                                Err(error) => Some(format!("Future queue error channel closed: {error}")),
                            };
                            if let Some(message) = message {
                                reporter_for_processor.set_status(
                                    ComponentStatus::Error,
                                    Some(format!("Future queue polling failed: {message}")),
                                ).await;
                                break;
                            }
                        }

                        // Dequeue events from priority queue (blocks until available)
                        arc_event = priority_queue.dequeue() => {
                            // Try to extract without cloning if we have sole ownership (zero-copy path).
                            let parts =
                                match SourceEventWrapper::try_unwrap_arc(arc_event) {
                                    Ok(parts) => parts,
                                    Err(arc) => {
                                        crate::channels::events::SourceEventParts {
                                            source_id: arc.source_id.clone(),
                                            event: arc.event.clone(),
                                            timestamp: arc.timestamp,
                                            profiling: arc.profiling.clone(),
                                            sequence: arc.sequence,
                                            source_position: arc.source_position.clone(),
                                        }
                                    }
                                };
                            let source_id = parts.source_id;
                            let event = parts.event;
                            let profiling_opt = parts.profiling;
                            let sequence = parts.sequence;
                            let source_position = parts.source_position;

                            debug!("Query '{query_id}' processing event from source '{source_id}'");

                            // Dedup: skip events already processed for this source
                            if dedup.should_skip(&source_id, sequence) {
                                debug!(
                                    "Query '{query_id}' skipping duplicate event from '{source_id}' (seq={seq}, checkpoint={cp})",
                                    seq = sequence.unwrap_or(0),
                                    cp = dedup.checkpoint_for(&source_id).unwrap_or(0)
                                );
                                continue;
                            }

                            match event {
                                SourceEvent::Control(SourceControl::FuturesDue) => {
                                    loop {
                                        let acquired = tokio::select! {
                                            biased;
                                            _ = &mut shutdown_rx => break 'events,
                                            root = acquire_query_root(session_control_for_processor.clone()) => root,
                                        };
                                        let processed: Result<bool> = async {
                                            let mut root = acquired?;
                                            let started = std::time::Instant::now();
                                            let mut output = None;
                                            let provisional = continuous_query_for_processor
                                                .process_due_futures_in_with_hook(&mut root, |due| {
                                                    output = due.as_ref().map(|due| (
                                                        convert_query_results(&due.results),
                                                        due.source_id.clone(),
                                                    ));
                                                    async { Ok(()) }
                                                })
                                                .await?;
                                            let staged = match output {
                                                Some((diffs, source_id)) => output_delivery.stage(
                                                    diffs,
                                                    &source_id,
                                                    crate::profiling::ProfilingMetadata::new(),
                                                    &root,
                                                    started,
                                                ).await?,
                                                None => None,
                                            };
                                            let receipt = root.commit_with_receipt().await?;
                                            let due = provisional.into_committed(&receipt)?;
                                            if let Some(staged) = staged {
                                                output_delivery.publish(staged, &receipt).await?;
                                            }
                                            Ok(due.is_some())
                                        }.await;
                                        match processed {
                                            Ok(true) => {}
                                            Ok(false) => break,
                                            Err(e) => {
                                                error!("Query '{query_id}' failed to process due futures: {e}");
                                                reporter_for_processor.set_status(
                                                    ComponentStatus::Error,
                                                    Some(format!("Future processing failed: {e}")),
                                                ).await;
                                                break 'events;
                                            }
                                        }
                                    }
                                    continue;
                                }
                                SourceEvent::Change(source_change) => {
                                    let Some(selection) = source_selections.get(&source_id) else {
                                        reporter_for_processor.set_status(
                                            ComponentStatus::Error,
                                            Some(format!("Received an event from unconfigured source '{source_id}'")),
                                        ).await;
                                        break;
                                    };
                                    let source_change = drasi_core::models::SourceInput::with_normalizer(
                                        source_change,
                                        selection.clone(),
                                    );
                                    let mut profiling =
                                        profiling_opt.unwrap_or_else(crate::profiling::ProfilingMetadata::new);
                                    profiling.query_receive_ns = Some(crate::profiling::timestamp_ns());
                                    let acquired = tokio::select! {
                                        biased;
                                        _ = &mut shutdown_rx => break 'events,
                                        root = acquire_query_root(session_control_for_processor.clone()) => root,
                                    };
                                    let processed: Result<()> = async {
                                        let mut root = acquired?;
                                        let started = std::time::Instant::now();
                                        profiling.query_core_call_ns = Some(crate::profiling::timestamp_ns());
                                        let mut diffs = Vec::new();
                                        let provisional = continuous_query_for_processor
                                            .process_source_change_in_with_hook(
                                                &mut root,
                                                source_change,
                                                |results| {
                                                    diffs = convert_query_results(results);
                                                    async { Ok(()) }
                                                },
                                            ).await?;
                                        profiling.query_core_return_ns = Some(crate::profiling::timestamp_ns());
                                        if let Some(sequence) = sequence {
                                            let position = source_position.as_ref().filter(|position|
                                                position.len() <= crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES
                                            );
                                            root.mark_dirty()?;
                                            checkpoint_store_for_processor
                                                .stage_checkpoint(&source_id, sequence, position)
                                                .await?;
                                        }
                                        profiling.query_send_ns = Some(crate::profiling::timestamp_ns());
                                        let staged = output_delivery.stage(
                                            diffs, &source_id, profiling, &root, started,
                                        ).await?;
                                        let receipt = root.commit_with_receipt().await?;
                                        provisional.into_committed(&receipt)?;
                                        if let Some(staged) = staged {
                                            output_delivery.publish(staged, &receipt).await?;
                                        }
                                        Ok(())
                                    }.await;
                                    match processed {
                                        Ok(()) => {
                                            if let Some(seq) = sequence {
                                                dedup.advance(&source_id, seq);

                                                if let Some(handle) = position_handles_for_processor.get(&source_id) {
                                                    handle.store(seq, std::sync::atomic::Ordering::Release);
                                                }
                                            }

                                        }
                                        Err(e) => {
                                            error!("Query '{query_id}' failed to process source change: {e}");
                                            reporter_for_processor.set_status(
                                                ComponentStatus::Error,
                                                Some(format!("Source '{source_id}' processing failed: {e}")),
                                            ).await;
                                            break 'events;
                                        }
                                    }
                                }
                                SourceEvent::Control(_) => {
                                    debug!("Query '{query_id}' ignoring control event from source '{source_id}'");
                                    continue;
                                }
                            }
                        }
                    }
                }

                fq_source_for_processor.stop().await;

            info!("Query '{query_id}' processing task exited");
        }
        .instrument(span),
    );

        // Store the task handle
        *task_handle_clone.write().await = Some(handle);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
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
                ComponentStatus::Running | ComponentStatus::Starting | ComponentStatus::Stopping
            ),
            "DrasiQuery::stop() called but local handle is not in expected pre-stop state"
        );
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping query".to_string()),
            )
            .await;

        self.quiesce().await?;

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
        self.subscription_tasks.read().await.len()
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

        if let Err(error) = self.quiesce().await {
            error!(
                "Query '{}' cannot release active index handles: {error:#}",
                self.base.config.id
            );
            return;
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

            crate::managers::lifecycle_helpers::reconfigure_component::<Arc<dyn Query>, _, _, _>(
                &self.graph,
                &id,
                "query",
                &old_query,
                || async {},
                || self.provision_query(new_config),
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
        let query = {
            let graph = self.graph.read().await;
            graph.get_runtime::<Arc<dyn Query>>(&id).cloned()
        };
        let query_config = query.as_ref().map(|query| query.get_config().clone());

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
        if let Some(query) = query {
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
                    let created = self.index_factory.build(backend_ref, &id).await
                        .with_context(|| format!("Query '{id}' was removed but its indexes could not be opened for cleanup"))?;
                    let guard = SessionGuard::begin(created.set.session_control.clone())
                        .await
                        .context("Failed to begin query removal cleanup")?;
                    guard.mark_dirty().context("Removal root is not active")?;
                    clear_persistent_indexes(
                        &id,
                        &Some(created.set.element_index),
                        &Some(created.set.archive_index),
                        &Some(created.set.result_index),
                        &Some(created.set.future_queue),
                    )
                    .await
                    .context("Failed to clear removed query indexes")?;
                    if let Some(checkpoint_store) = created.checkpoint_store {
                        checkpoint_store
                            .clear_checkpoints()
                            .await
                            .context("Failed to clear removed query checkpoints")?;
                        checkpoint_store
                            .write_result_sequence(&id, 0)
                            .await
                            .context("Failed to reset removed query output sequence")?;
                    }
                    if let Some(writer) = created.outbox_writer {
                        writer
                            .clear(&id)
                            .await
                            .context("Failed to clear removed query outbox")?;
                    }
                    if let Some(writer) = created.live_results_writer {
                        writer
                            .clear(&id)
                            .await
                            .context("Failed to clear removed query snapshot")?;
                    }
                    guard
                        .commit()
                        .await
                        .context("Failed to commit query removal cleanup")?;
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

    /// Stop all currently running or starting queries.
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
                            ComponentStatus::Running | ComponentStatus::Starting
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
