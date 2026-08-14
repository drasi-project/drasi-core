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
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

// Import drasi-core components
use drasi_core::{
    evaluation::context::{QueryPartEvaluationContext, QueryVariables},
    evaluation::functions::FunctionRegistry,
    evaluation::variable_value::VariableValue,
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
use crate::queries::PriorityQueue;
use crate::queries::QueryBase;
use crate::sources::FutureQueueSource;
use crate::sources::Source;
use crate::sources::SourceManager;
use crate::state_store::{StateStoreCompareAndSwapResult, StateStoreError, StateStoreProvider};
use tracing::Instrument;

const QUERY_OUTPUT_SEQUENCE_STORE_ID: &str = "__drasi_query_output_sequences";
const QUERY_OUTPUT_OUTBOX_STORE_PREFIX: &str = "__drasi_query_outbox:";
const QUERY_OUTPUT_LIVE_RESULTS_STORE_PREFIX: &str = "__drasi_query_live_results:";
const QUERY_OUTPUT_OUTBOX_CONFIG_KEY: &str = "__config_hash";
const QUERY_OUTPUT_SNAPSHOT_SEQUENCE_KEY: &str = "__snapshot_sequence";
const QUERY_OUTPUT_RESET_KEY_PREFIX: &str = "reset:";

struct StateStoreQueryOutputWriter {
    store: Arc<dyn StateStoreProvider>,
    query_id: String,
    store_id: String,
    live_results_store_id: String,
    config_hash: u64,
    reset_baseline: u64,
}

impl StateStoreQueryOutputWriter {
    fn new(
        store: Arc<dyn StateStoreProvider>,
        query_id: &str,
        config_hash: u64,
        reset_baseline: u64,
    ) -> Self {
        Self {
            store,
            query_id: query_id.to_string(),
            store_id: format!("{QUERY_OUTPUT_OUTBOX_STORE_PREFIX}{query_id}"),
            live_results_store_id: format!("{QUERY_OUTPUT_LIVE_RESULTS_STORE_PREFIX}{query_id}"),
            config_hash,
            reset_baseline,
        }
    }

    fn sequence_key(sequence: u64) -> String {
        format!("{sequence:020}")
    }

    async fn prepare(&self) -> Result<bool> {
        let expected = self.config_hash.to_le_bytes();
        match self
            .store
            .get(&self.store_id, QUERY_OUTPUT_OUTBOX_CONFIG_KEY)
            .await?
        {
            Some(stored) if stored.as_slice() == expected => Ok(false),
            Some(_) => {
                write_durable_output_reset(
                    self.store.as_ref(),
                    &self.query_id,
                    self.config_hash,
                    self.reset_baseline,
                )
                .await?;
                self.store.clear_store(&self.store_id).await?;
                self.store.clear_store(&self.live_results_store_id).await?;
                self.store
                    .set(
                        &self.store_id,
                        QUERY_OUTPUT_OUTBOX_CONFIG_KEY,
                        expected.to_vec(),
                    )
                    .await?;
                Ok(true)
            }
            None => {
                self.store
                    .set(
                        &self.store_id,
                        QUERY_OUTPUT_OUTBOX_CONFIG_KEY,
                        expected.to_vec(),
                    )
                    .await?;
                Ok(false)
            }
        }
    }

    async fn sequence_keys(&self) -> Result<Vec<(u64, String)>, drasi_core::interface::IndexError> {
        let mut keys = self
            .store
            .list_keys(&self.store_id)
            .await
            .map_err(drasi_core::interface::IndexError::other)?
            .into_iter()
            .filter_map(|key| key.parse::<u64>().ok().map(|sequence| (sequence, key)))
            .collect::<Vec<_>>();
        keys.sort_unstable_by_key(|(sequence, _)| *sequence);
        Ok(keys)
    }

    async fn live_result_keys(
        &self,
    ) -> Result<Vec<(u64, String)>, drasi_core::interface::IndexError> {
        let mut keys = self
            .store
            .list_keys(&self.live_results_store_id)
            .await
            .map_err(drasi_core::interface::IndexError::other)?
            .into_iter()
            .filter_map(|key| key.parse::<u64>().ok().map(|signature| (signature, key)))
            .collect::<Vec<_>>();
        keys.sort_unstable_by_key(|(signature, _)| *signature);
        Ok(keys)
    }
}

#[async_trait]
impl OutboxWriter for StateStoreQueryOutputWriter {
    async fn append(
        &self,
        _query_id: &str,
        sequence: u64,
        data: &[u8],
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.store
            .set(&self.store_id, &Self::sequence_key(sequence), data.to_vec())
            .await
            .map_err(drasi_core::interface::IndexError::other)
    }

    async fn read_from(
        &self,
        _query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, drasi_core::interface::IndexError> {
        let keys = self
            .sequence_keys()
            .await?
            .into_iter()
            .filter(|(sequence, _)| *sequence > after_sequence)
            .collect::<Vec<_>>();
        let key_refs = keys.iter().map(|(_, key)| key.as_str()).collect::<Vec<_>>();
        let values = self
            .store
            .get_many(&self.store_id, &key_refs)
            .await
            .map_err(drasi_core::interface::IndexError::other)?;

        keys.into_iter()
            .map(|(sequence, key)| {
                values
                    .get(&key)
                    .cloned()
                    .map(|data| (sequence, data))
                    .ok_or(drasi_core::interface::IndexError::CorruptedData)
            })
            .collect()
    }

    async fn read_latest_sequence(
        &self,
        _query_id: &str,
    ) -> Result<Option<u64>, drasi_core::interface::IndexError> {
        Ok(self
            .sequence_keys()
            .await?
            .last()
            .map(|(sequence, _)| *sequence))
    }

    async fn clear(&self, _query_id: &str) -> Result<(), drasi_core::interface::IndexError> {
        self.store
            .clear_store(&self.store_id)
            .await
            .map_err(drasi_core::interface::IndexError::other)?;
        self.store
            .set(
                &self.store_id,
                QUERY_OUTPUT_OUTBOX_CONFIG_KEY,
                self.config_hash.to_le_bytes().to_vec(),
            )
            .await
            .map_err(drasi_core::interface::IndexError::other)?;
        let mut reset = Vec::with_capacity(16);
        reset.extend_from_slice(&self.config_hash.to_le_bytes());
        reset.extend_from_slice(&self.reset_baseline.to_le_bytes());
        self.store
            .set(
                QUERY_OUTPUT_SEQUENCE_STORE_ID,
                &format!("{QUERY_OUTPUT_RESET_KEY_PREFIX}{}", self.query_id),
                reset,
            )
            .await
            .map_err(drasi_core::interface::IndexError::other)
    }

    async fn trim_to_capacity(
        &self,
        _query_id: &str,
        capacity: usize,
    ) -> Result<usize, drasi_core::interface::IndexError> {
        let keys = self.sequence_keys().await?;
        let remove_count = keys.len().saturating_sub(capacity);
        if remove_count == 0 {
            return Ok(0);
        }
        let remove = keys
            .iter()
            .take(remove_count)
            .map(|(_, key)| key.as_str())
            .collect::<Vec<_>>();
        self.store
            .delete_many(&self.store_id, &remove)
            .await
            .map_err(drasi_core::interface::IndexError::other)
    }
}

#[async_trait]
impl LiveResultsWriter for StateStoreQueryOutputWriter {
    async fn apply_mutations(
        &self,
        _query_id: &str,
        mutations: &[drasi_core::interface::RowMutation<'_>],
    ) -> Result<(), drasi_core::interface::IndexError> {
        let mut deletes = Vec::new();
        for mutation in mutations {
            let key = Self::sequence_key(mutation.row_signature);
            if let Some(data) = mutation.data {
                self.store
                    .set(&self.live_results_store_id, &key, data.to_vec())
                    .await
                    .map_err(drasi_core::interface::IndexError::other)?;
            } else {
                deletes.push(key);
            }
        }
        if !deletes.is_empty() {
            let delete_refs = deletes.iter().map(String::as_str).collect::<Vec<_>>();
            self.store
                .delete_many(&self.live_results_store_id, &delete_refs)
                .await
                .map_err(drasi_core::interface::IndexError::other)?;
        }
        Ok(())
    }

    async fn read_snapshot(
        &self,
        _query_id: &str,
    ) -> Result<Vec<(u64, Vec<u8>)>, drasi_core::interface::IndexError> {
        let keys = self.live_result_keys().await?;
        let key_refs = keys.iter().map(|(_, key)| key.as_str()).collect::<Vec<_>>();
        let values = self
            .store
            .get_many(&self.live_results_store_id, &key_refs)
            .await
            .map_err(drasi_core::interface::IndexError::other)?;
        keys.into_iter()
            .map(|(signature, key)| {
                values
                    .get(&key)
                    .cloned()
                    .map(|data| (signature, data))
                    .ok_or(drasi_core::interface::IndexError::CorruptedData)
            })
            .collect()
    }

    async fn clear(&self, _query_id: &str) -> Result<(), drasi_core::interface::IndexError> {
        self.store
            .clear_store(&self.live_results_store_id)
            .await
            .map(|_| ())
            .map_err(drasi_core::interface::IndexError::other)
    }

    async fn row_count(&self, _query_id: &str) -> Result<usize, drasi_core::interface::IndexError> {
        Ok(self.live_result_keys().await?.len())
    }

    async fn read_snapshot_sequence(
        &self,
        _query_id: &str,
    ) -> Result<Option<u64>, drasi_core::interface::IndexError> {
        let Some(bytes) = self
            .store
            .get(
                &self.live_results_store_id,
                QUERY_OUTPUT_SNAPSHOT_SEQUENCE_KEY,
            )
            .await
            .map_err(drasi_core::interface::IndexError::other)?
        else {
            return Ok(None);
        };
        let encoded: [u8; 8] = bytes
            .try_into()
            .map_err(|_| drasi_core::interface::IndexError::CorruptedData)?;
        Ok(Some(u64::from_le_bytes(encoded)))
    }

    async fn write_snapshot_sequence(
        &self,
        _query_id: &str,
        sequence: u64,
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.store
            .set(
                &self.live_results_store_id,
                QUERY_OUTPUT_SNAPSHOT_SEQUENCE_KEY,
                sequence.to_le_bytes().to_vec(),
            )
            .await
            .map_err(drasi_core::interface::IndexError::other)
    }
}

async fn read_durable_output_sequence(
    store: &dyn StateStoreProvider,
    query_id: &str,
) -> Result<Option<u64>> {
    let Some(bytes) = store.get(QUERY_OUTPUT_SEQUENCE_STORE_ID, query_id).await? else {
        return Ok(None);
    };
    let encoded: [u8; 8] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        anyhow::anyhow!(
            "Invalid durable output sequence for query '{query_id}': expected 8 bytes, got {}",
            bytes.len()
        )
    })?;
    Ok(Some(u64::from_le_bytes(encoded)))
}

async fn write_durable_output_sequence(
    store: &dyn StateStoreProvider,
    query_id: &str,
    sequence: u64,
) -> Result<()> {
    let new_value = sequence.to_le_bytes();
    for _ in 0..8 {
        let existing = store.get(QUERY_OUTPUT_SEQUENCE_STORE_ID, query_id).await?;
        if let Some(bytes) = existing.as_ref() {
            let encoded: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
                anyhow::anyhow!(
                    "Invalid durable output sequence for query '{query_id}': expected 8 bytes, got {}",
                    bytes.len()
                )
            })?;
            if u64::from_le_bytes(encoded) >= sequence {
                return Ok(());
            }
        }

        match store
            .compare_and_swap(
                QUERY_OUTPUT_SEQUENCE_STORE_ID,
                query_id,
                existing.as_deref(),
                new_value.to_vec(),
            )
            .await
        {
            Ok(StateStoreCompareAndSwapResult::Swapped) => return Ok(()),
            Ok(StateStoreCompareAndSwapResult::Mismatch) => continue,
            Err(StateStoreError::Unsupported(_)) => {
                // Legacy providers are serialized by the query output-state
                // lock within one runtime, so a direct durable write retains
                // the pre-CAS behavior for them.
                store
                    .set(QUERY_OUTPUT_SEQUENCE_STORE_ID, query_id, new_value.to_vec())
                    .await?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow::anyhow!(
        "Failed to persist durable output sequence for query '{query_id}' after repeated CAS retries"
    ))
}

async fn write_durable_output_reset(
    store: &dyn StateStoreProvider,
    query_id: &str,
    config_hash: u64,
    sequence: u64,
) -> Result<()> {
    let mut value = Vec::with_capacity(16);
    value.extend_from_slice(&config_hash.to_le_bytes());
    value.extend_from_slice(&sequence.to_le_bytes());
    store
        .set(
            QUERY_OUTPUT_SEQUENCE_STORE_ID,
            &format!("{QUERY_OUTPUT_RESET_KEY_PREFIX}{query_id}"),
            value,
        )
        .await?;
    Ok(())
}

async fn read_durable_output_reset(
    store: &dyn StateStoreProvider,
    query_id: &str,
    config_hash: u64,
) -> Result<Option<u64>> {
    let Some(bytes) = store
        .get(
            QUERY_OUTPUT_SEQUENCE_STORE_ID,
            &format!("{QUERY_OUTPUT_RESET_KEY_PREFIX}{query_id}"),
        )
        .await?
    else {
        return Ok(None);
    };
    if bytes.len() != 16 {
        anyhow::bail!(
            "Invalid durable output reset marker for query '{query_id}': expected 16 bytes, got {}",
            bytes.len()
        );
    }
    let stored_hash = u64::from_le_bytes(bytes[..8].try_into().expect("validated reset marker"));
    if stored_hash != config_hash {
        return Ok(None);
    }
    Ok(Some(u64::from_le_bytes(
        bytes[8..].try_into().expect("validated reset marker"),
    )))
}

fn serialize_live_result_mutations(result: &QueryResult) -> Result<Vec<(u64, Option<Vec<u8>>)>> {
    result
        .results
        .iter()
        .filter_map(|diff| match diff {
            ResultDiff::Add {
                data,
                row_signature,
            } => Some((*row_signature, Some(data))),
            ResultDiff::Update {
                after,
                row_signature,
                ..
            }
            | ResultDiff::Aggregation {
                after,
                row_signature,
                ..
            } => Some((*row_signature, Some(after))),
            ResultDiff::Delete { row_signature, .. } => Some((*row_signature, None)),
            ResultDiff::Noop => None,
        })
        .map(|(row_signature, data)| {
            let data = data.map(rmp_serde::to_vec).transpose().with_context(|| {
                format!(
                    "Failed to serialize live result row {row_signature} for query '{}'",
                    result.query_id
                )
            })?;
            Ok((row_signature, data))
        })
        .collect()
}

async fn persist_live_result(
    writer: &dyn LiveResultsWriter,
    query_id: &str,
    result: &QueryResult,
) -> Result<()> {
    let serialized_data = serialize_live_result_mutations(result)?;
    let row_mutations: Vec<drasi_core::interface::RowMutation<'_>> = serialized_data
        .iter()
        .map(|(row_signature, data)| drasi_core::interface::RowMutation {
            row_signature: *row_signature,
            data: data.as_deref(),
        })
        .collect();

    if !row_mutations.is_empty() {
        writer
            .apply_mutations(query_id, &row_mutations)
            .await
            .context("Failed to persist live result mutations")?;
    }
    writer
        .write_snapshot_sequence(query_id, result.sequence)
        .await
        .context("Failed to persist live result sequence")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn publish_bootstrap_output(
    query_id: &str,
    output_state: &Arc<RwLock<QueryOutputState>>,
    pre_bootstrap_results: &im::HashMap<u64, serde_json::Value>,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
    outbox_writer: &Option<Arc<dyn OutboxWriter>>,
    live_results_writer: &Option<Arc<dyn LiveResultsWriter>>,
    checkpoint_store: &Arc<dyn CheckpointStore>,
    output_sequence_store: &Option<Arc<dyn StateStoreProvider>>,
    outbox_capacity: usize,
    output_metrics: &Arc<QueryOutputMetrics>,
) -> Result<()> {
    let (result, previous_sequence) = {
        let state = output_state.read().await;
        let current_results = state.clone_results();
        let mut diffs = Vec::new();
        for (row_signature, before) in pre_bootstrap_results {
            match current_results.get(row_signature) {
                None => diffs.push(ResultDiff::Delete {
                    data: before.clone(),
                    row_signature: *row_signature,
                }),
                Some(after) if after != before => diffs.push(ResultDiff::Update {
                    data: after.clone(),
                    before: before.clone(),
                    after: after.clone(),
                    grouping_keys: None,
                    row_signature: *row_signature,
                }),
                Some(_) => {}
            }
        }
        for (row_signature, data) in &current_results {
            if !pre_bootstrap_results.contains_key(row_signature) {
                diffs.push(ResultDiff::Add {
                    data: data.clone(),
                    row_signature: *row_signature,
                });
            }
        }
        diffs.sort_unstable_by_key(|diff| match diff {
            ResultDiff::Add { row_signature, .. }
            | ResultDiff::Delete { row_signature, .. }
            | ResultDiff::Update { row_signature, .. }
            | ResultDiff::Aggregation { row_signature, .. } => *row_signature,
            ResultDiff::Noop => 0,
        });
        if diffs.is_empty() {
            return Ok(());
        }
        let sequence = state
            .as_of_sequence()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Bootstrap output sequence overflow"))?;

        let mut metadata = HashMap::new();
        metadata.insert(
            "bootstrap_snapshot".to_string(),
            serde_json::Value::Bool(true),
        );
        metadata.insert(
            "result_count".to_string(),
            serde_json::Value::Number(diffs.len().into()),
        );
        let result = Arc::new(QueryResult::new(
            query_id.to_string(),
            sequence,
            chrono::Utc::now(),
            diffs,
            metadata,
        ));
        (result, state.as_of_sequence())
    };

    if let Some(writer) = outbox_writer {
        let data = rmp_serde::to_vec_named(result.as_ref())
            .context("Failed to serialize bootstrap query output")?;
        writer
            .append(query_id, result.sequence, &data)
            .await
            .context("Failed to persist bootstrap query output")?;
    }
    if let Some(writer) = live_results_writer {
        persist_live_result(writer.as_ref(), query_id, result.as_ref())
            .await
            .context("Failed to persist bootstrap live results")?;
    }
    if !advance_result_sequence_if_contiguous(checkpoint_store.as_ref(), query_id, result.sequence)
        .await?
    {
        anyhow::bail!(
            "Bootstrap output sequence {} for query '{query_id}' is not contiguous",
            result.sequence
        );
    }
    if let Some(store) = output_sequence_store {
        write_durable_output_sequence(store.as_ref(), query_id, result.sequence).await?;
    }
    if let Some(writer) = outbox_writer {
        if let Err(error) = writer.trim_to_capacity(query_id, outbox_capacity).await {
            warn!("Query '{query_id}' failed to trim bootstrap query outbox: {error}");
        }
    }

    {
        let mut state = output_state.write().await;
        if state.as_of_sequence() != previous_sequence {
            anyhow::bail!(
                "Bootstrap output state for query '{query_id}' advanced concurrently from sequence {previous_sequence} to {}",
                state.as_of_sequence()
            );
        }
        let pushed = state.advance_sequence_and_push(result.as_ref().clone());
        debug_assert_eq!(pushed.sequence, result.sequence);
        output_metrics.record_seq_advance();
        output_metrics.record_live_results_count(state.results_len());
        output_metrics.update_outbox(
            state.outbox_len(),
            state.outbox_earliest_seq().unwrap_or(0),
            state.as_of_sequence(),
        );
    }

    let dispatchers = dispatchers.read().await;
    for dispatcher in dispatchers.iter() {
        dispatcher
            .dispatch_change(result.clone())
            .await
            .context("Failed to dispatch bootstrap query output")?;
    }
    Ok(())
}

async fn advance_result_sequence_if_contiguous(
    store: &dyn CheckpointStore,
    query_id: &str,
    sequence: u64,
) -> Result<bool> {
    let current = store
        .read_result_sequence(query_id)
        .await
        .context("Failed to read persisted result sequence")?;

    if current.is_some_and(|current| current >= sequence) {
        return Ok(true);
    }

    let is_contiguous = match current {
        Some(current) => current.checked_add(1) == Some(sequence),
        None => sequence <= 1,
    };
    if !is_contiguous {
        return Ok(false);
    }

    store
        .write_result_sequence(query_id, sequence)
        .await
        .context("Failed to write persisted result sequence")?;
    Ok(true)
}

fn decode_persisted_outbox(
    query_id: &str,
    latest_sequence: Option<u64>,
    entries: Vec<(u64, Vec<u8>)>,
) -> Result<Vec<Arc<QueryResult>>> {
    let mut decoded = Vec::with_capacity(entries.len());
    let mut previous_sequence: Option<u64> = None;
    for (stored_sequence, data) in entries {
        if previous_sequence
            .is_some_and(|previous| previous.checked_add(1) != Some(stored_sequence))
        {
            anyhow::bail!(
                "Persistent outbox for query '{query_id}' is not contiguous at sequence {stored_sequence}"
            );
        }
        let result: QueryResult = rmp_serde::from_slice(&data).with_context(|| {
            format!(
                "Failed to deserialize persistent outbox entry {stored_sequence} for query '{query_id}'"
            )
        })?;
        if result.query_id != query_id || result.sequence != stored_sequence {
            anyhow::bail!(
                "Persistent outbox entry {stored_sequence} does not match query '{query_id}' and its storage key"
            );
        }
        previous_sequence = Some(stored_sequence);
        decoded.push(Arc::new(result));
    }
    if latest_sequence != previous_sequence {
        anyhow::bail!(
            "Persistent outbox latest sequence for query '{query_id}' does not match its entries"
        );
    }
    Ok(decoded)
}

fn ensure_persistent_outbox_reaches(
    query_id: &str,
    latest_sequence: Option<u64>,
    reset_sequence: Option<u64>,
    sequence_baseline: u64,
) -> Result<()> {
    let certified_sequence = latest_sequence
        .unwrap_or(0)
        .max(reset_sequence.unwrap_or(0));
    if sequence_baseline > certified_sequence {
        anyhow::bail!(
            "Persistent outbox for query '{query_id}' is certified only through sequence {certified_sequence} below the restart baseline {sequence_baseline}; refusing to discard valid replay history across a durable gap"
        );
    }
    Ok(())
}

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
    use super::{
        advance_result_sequence_if_contiguous, convert_variable_value_to_json,
        decode_persisted_outbox, ensure_persistent_outbox_reaches,
    };
    use crate::channels::{QueryResult, ResultDiff};
    use chrono::{Duration as ChronoDuration, FixedOffset, NaiveDate, NaiveTime, TimeZone};
    use drasi_core::evaluation::variable_value::{
        duration::Duration as VarDuration, zoned_datetime::ZonedDateTime as VarZonedDateTime,
        zoned_time::ZonedTime as VarZonedTime, VariableValue,
    };
    use drasi_core::in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore;
    use drasi_core::interface::CheckpointStore;
    use std::collections::HashMap;

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

    fn encoded_result(sequence: u64) -> Vec<u8> {
        rmp_serde::to_vec(&QueryResult::with_profiling(
            "q1".to_string(),
            sequence,
            chrono::Utc::now(),
            vec![ResultDiff::Noop],
            HashMap::new(),
            crate::profiling::ProfilingMetadata::new(),
        ))
        .expect("serialize query result")
    }

    #[test]
    fn rejects_non_contiguous_persistent_outbox() {
        let error = decode_persisted_outbox(
            "q1",
            Some(3),
            vec![(1, encoded_result(1)), (3, encoded_result(3))],
        )
        .expect_err("outbox gap must be rejected");

        assert!(
            error.to_string().contains("not contiguous"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn rejects_persistent_outbox_tail_gap_without_deleting_prefix() {
        let error = ensure_persistent_outbox_reaches("q1", Some(4), None, 5)
            .expect_err("outbox tail gap must fail closed");

        assert!(error.to_string().contains("only through sequence 4"));
    }

    #[test]
    fn accepts_intentional_output_reset_at_restart_baseline() {
        ensure_persistent_outbox_reaches("q1", None, Some(5), 5)
            .expect("durable reset marker certifies an intentionally empty outbox");
    }

    #[tokio::test]
    async fn persisted_result_sequence_advances_only_contiguously() {
        let store = InMemoryCheckpointStore::new();
        store.write_result_sequence("q1", 3).await.unwrap();

        assert!(!advance_result_sequence_if_contiguous(&store, "q1", 5)
            .await
            .unwrap());
        assert_eq!(store.read_result_sequence("q1").await.unwrap(), Some(3));

        assert!(advance_result_sequence_if_contiguous(&store, "q1", 4)
            .await
            .unwrap());
        assert!(advance_result_sequence_if_contiguous(&store, "q1", 5)
            .await
            .unwrap());
        assert_eq!(store.read_result_sequence("q1").await.unwrap(), Some(5));
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

    /// Ensure result sequences remain comparable with durable reaction
    /// checkpoints from an earlier process.
    ///
    /// Query implementations without a process-local sequence source may keep
    /// the default no-op implementation.
    async fn ensure_output_sequence_at_least(&self, _minimum: u64) -> Result<()> {
        Ok(())
    }

    /// Restore any query-owned durable sequence baseline before merging
    /// reaction checkpoints.
    async fn restore_output_sequence_baseline(&self) -> Result<()> {
        Ok(())
    }

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

/// Dispatch query evaluation results to the current result set and all subscribed reactions.
///
/// Shared between the regular event processing path and the future queue drain path.
/// Uses `QueryOutputState` for O(1) result-set updates keyed by `row_signature`,
/// increments the sequence counter, and pushes to the outbox ring buffer.
#[allow(clippy::too_many_arguments)]
async fn dispatch_query_results(
    results: &[QueryPartEvaluationContext],
    source_id: &str,
    query_id: &str,
    output_state: &RwLock<QueryOutputState>,
    dispatchers: &RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>,
    outbox_writer: &Option<Arc<dyn OutboxWriter>>,
    live_results_writer: &Option<Arc<dyn LiveResultsWriter>>,
    checkpoint_store: &Option<Arc<dyn CheckpointStore>>,
    output_sequence_store: &Option<Arc<dyn StateStoreProvider>>,
    outbox_capacity: usize,
    profiling: crate::profiling::ProfilingMetadata,
    output_metrics: &Arc<QueryOutputMetrics>,
) {
    // Convert Drasi results to our QueryResult format, filtering out Noops
    let converted_results: Vec<ResultDiff> = results
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
        .collect();

    // If all results were Noops, skip outbox/sequence advancement and dispatch
    if converted_results.is_empty() {
        return;
    }

    // Apply diffs to the output state, build QueryResult, increment sequence,
    // push to outbox, and get back the Arc for zero-copy dispatch — all in one
    // write-lock acquisition.
    let arc_result = {
        let tx_start = std::time::Instant::now();
        let mut state = output_state.write().await;
        state.apply_diffs(&converted_results);

        let result_count = converted_results.len();
        let query_result = QueryResult::with_profiling(
            query_id.to_string(),
            0, // sequence assigned by advance_sequence_and_push
            chrono::Utc::now(),
            converted_results,
            {
                let mut meta = HashMap::new();
                meta.insert(
                    "source_id".to_string(),
                    serde_json::Value::String(source_id.to_string()),
                );
                meta.insert(
                    "processed_by".to_string(),
                    serde_json::Value::String("drasi-core".to_string()),
                );
                meta.insert(
                    "result_count".to_string(),
                    serde_json::Value::Number(result_count.into()),
                );
                meta
            },
            profiling,
        );

        let result = state.advance_sequence_and_push(query_result);

        // Persist the sequence clock before publishing the result. Reaction
        // checkpoints live in this same durable store, so a new process must
        // never restart result numbering below an acknowledged checkpoint.
        // Keep the output-state lock across this write so legacy rebasing and
        // concurrent result production cannot reorder sequence persistence.
        if let Some(store) = output_sequence_store {
            if let Err(e) =
                write_durable_output_sequence(store.as_ref(), query_id, result.sequence).await
            {
                // Continue delivery to preserve availability. If a durable
                // reaction acknowledges this result, its checkpoint is read as
                // a fallback floor before the next query start.
                warn!(
                    "Query '{query_id}' failed to persist durable output sequence {}: {e}",
                    result.sequence
                );
            }
        }

        // Update query output metrics
        let duration_ns = u64::try_from(tx_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        output_metrics.record_transaction_duration_ns(duration_ns);
        output_metrics.record_seq_advance();
        output_metrics.record_live_results_count(state.results_len());
        let earliest_seq = state.outbox_earliest_seq().unwrap_or(0);
        output_metrics.update_outbox(state.outbox_len(), earliest_seq, state.as_of_sequence());

        result
    };

    // Persist to outbox and live results writers if available (best-effort).
    // These writes are NOT transactional with the index updates — on crash between
    // index commit and outbox write, reactions will re-read from checkpoint sequence.
    let mut outbox_ok = true;
    if let Some(writer) = outbox_writer {
        // Serialize the QueryResult for the outbox using MessagePack (compact binary)
        match rmp_serde::to_vec_named(arc_result.as_ref()) {
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
    }

    let mut live_results_ok = true;
    if let Some(writer) = live_results_writer {
        if let Err(e) = persist_live_result(writer.as_ref(), query_id, arc_result.as_ref()).await {
            warn!(
                "Query '{query_id}' failed to persist live results for seq={}: {e:#}",
                arc_result.sequence
            );
            live_results_ok = false;
        }
    }

    // Record the last persisted result sequence only if BOTH the outbox and
    // live-results writes succeeded. Otherwise recovery may see this sequence
    // as durable while the actual data is missing.
    let mut result_sequence_is_contiguous = checkpoint_store.is_none();
    if outbox_ok && live_results_ok {
        if let Some(store) = checkpoint_store {
            match advance_result_sequence_if_contiguous(
                store.as_ref(),
                query_id,
                arc_result.sequence,
            )
            .await
            {
                Ok(true) => result_sequence_is_contiguous = true,
                Ok(false) => {
                    warn!(
                        "Query '{query_id}' did not advance persisted result sequence to {} because an earlier output is not durably complete",
                        arc_result.sequence
                    );
                }
                Err(e) => {
                    warn!(
                        "Query '{query_id}' failed to write result sequence {}: {e:#}",
                        arc_result.sequence
                    );
                }
            }
        }
    }

    // Never evict entries needed to repair a lagging live-result snapshot.
    // Once recovery advances the certified watermark, normal bounded trimming
    // resumes on the next result.
    if outbox_ok && result_sequence_is_contiguous {
        if let Some(writer) = outbox_writer {
            if let Err(e) = writer.trim_to_capacity(query_id, outbox_capacity).await {
                warn!("Query '{query_id}' failed to trim persistent outbox: {e}");
            }
        }
    }

    debug!(
        "Query '{query_id}' sending {} results to reactions (seq={})",
        arc_result.results.len(),
        arc_result.sequence
    );

    // Dispatch query result to all subscribed reactions
    let dispatchers = dispatchers.read().await;
    for dispatcher in dispatchers.iter() {
        if let Err(e) = dispatcher.dispatch_change(arc_result.clone()).await {
            debug!("Failed to dispatch result for query '{query_id}': {e}");
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
    // Shared state store used to make the query-result sequence clock durable,
    // even when the query's index backend itself is in-memory.
    output_sequence_store: Option<Arc<dyn StateStoreProvider>>,
    // True only when this process restored or established a restart baseline.
    // A sequence written by an early result in a legacy run does not set this:
    // the manager must still rebase that result above legacy reaction checkpoints.
    output_sequence_has_restart_baseline: AtomicBool,
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
}

impl DrasiQuery {
    pub fn new(
        instance_id: impl Into<String>,
        config: QueryConfig,
        source_manager: Arc<SourceManager>,
        index_factory: Arc<crate::indexes::IndexFactory>,
        middleware_registry: Arc<MiddlewareTypeRegistry>,
        default_recovery_policy: Option<crate::recovery::RecoveryPolicy>,
        output_sequence_store: Option<Arc<dyn StateStoreProvider>>,
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
            output_sequence_store,
            output_sequence_has_restart_baseline: AtomicBool::new(false),
            outbox_writer: Arc::new(RwLock::new(None)),
            live_results_writer: Arc::new(RwLock::new(None)),
            bootstrap_timeout,
            resolved_recovery_policy,
            subscribed_source_ids: Arc::new(RwLock::new(Vec::new())),
            output_metrics: Arc::new(QueryOutputMetrics::new()),
        })
    }

    async fn clear_persistent_output(&self) -> Result<()> {
        let outbox_writer = self.outbox_writer.read().await.clone();
        let live_results_writer = self.live_results_writer.read().await.clone();
        if let Some(writer) = outbox_writer {
            writer
                .clear(&self.base.config.id)
                .await
                .context("Failed to clear persistent query outbox")?;
        }
        if let Some(writer) = live_results_writer {
            writer
                .clear(&self.base.config.id)
                .await
                .context("Failed to clear persistent query live results")?;
        }
        Ok(())
    }

    async fn clear_output_data(&self) -> Result<()> {
        self.clear_persistent_output().await?;
        let mut state = self.output_state.write().await;
        state.clear_data_preserving_sequence();
        if let Some(store) = self.output_sequence_store.as_ref() {
            write_durable_output_reset(
                store.as_ref(),
                &self.base.config.id,
                self.config_hash,
                state.as_of_sequence(),
            )
            .await?;
        }
        self.output_metrics
            .update_outbox(0, 0, state.as_of_sequence());
        Ok(())
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

    async fn restore_output_sequence(&self) -> Result<()> {
        let Some(store) = self.output_sequence_store.as_ref() else {
            return Ok(());
        };
        let Some(sequence) =
            read_durable_output_sequence(store.as_ref(), &self.base.config.id).await?
        else {
            self.output_sequence_has_restart_baseline
                .store(false, Ordering::Release);
            return Ok(());
        };

        let mut state = self.output_state.write().await;
        let delta = sequence.saturating_sub(state.as_of_sequence());
        state
            .rebase_sequence(delta)
            .map_err(|e| anyhow::anyhow!("Failed to restore query output sequence: {e}"))?;
        self.output_sequence_has_restart_baseline
            .store(true, Ordering::Release);
        Ok(())
    }

    async fn establish_output_sequence_floor(&self, minimum: u64) -> Result<()> {
        let Some(store) = self.output_sequence_store.as_ref() else {
            return Ok(());
        };

        let mut state = self.output_state.write().await;
        let has_baseline = self
            .output_sequence_has_restart_baseline
            .load(Ordering::Acquire);
        let delta = if has_baseline {
            minimum.saturating_sub(state.as_of_sequence())
        } else {
            // No sequence key existed when this process started. This is legacy
            // state: retained results belong to the new process-local epoch and
            // must all be shifted above the old reaction checkpoint watermark.
            minimum
        };
        let sequence = state
            .as_of_sequence()
            .checked_add(delta)
            .ok_or_else(|| anyhow::anyhow!("Query output sequence overflow"))?;

        write_durable_output_sequence(store.as_ref(), &self.base.config.id, sequence).await?;
        state
            .rebase_sequence(delta)
            .map_err(|e| anyhow::anyhow!("Failed to establish query output sequence: {e}"))?;
        self.output_sequence_has_restart_baseline
            .store(true, Ordering::Release);
        Ok(())
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
        self.subscription_tasks.read().await.len()
    }

    /// Access the checkpoint store (for internal/test use only).
    #[doc(hidden)]
    pub async fn get_checkpoint_store(&self) -> Option<Arc<dyn CheckpointStore>> {
        self.checkpoint_store.read().await.clone()
    }
}

/// Clear all persistent indexes on config hash mismatch or hash read failure.
///
/// Called to remove stale element/archive/result/future data so that
/// the subsequent bootstrap runs against a clean state.
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

#[async_trait]
impl Query for DrasiQuery {
    async fn start(&self) -> Result<()> {
        log_component_start("Query", &self.base.config.id);

        self.restore_output_sequence().await?;
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
        let mut persistent_state_matches = false;

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
        };

        if let Some(store) = self.output_sequence_store.as_ref() {
            let needs_outbox = self.outbox_writer.read().await.is_none();
            let needs_live_results = self.live_results_writer.read().await.is_none();
            if needs_outbox || needs_live_results {
                let reset_baseline = self.output_state.read().await.as_of_sequence();
                let writer = Arc::new(StateStoreQueryOutputWriter::new(
                    store.clone(),
                    &self.base.config.id,
                    self.config_hash,
                    reset_baseline,
                ));
                writer
                    .prepare()
                    .await
                    .context("Failed to prepare durable query output storage")?;
                if needs_outbox {
                    *self.outbox_writer.write().await = Some(writer.clone());
                }
                if needs_live_results {
                    *self.live_results_writer.write().await = Some(writer);
                }
            }
        }

        // Persist the checkpoint_store for future stop/start cycles
        *self.checkpoint_store.write().await = Some(checkpoint_store.clone());

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
            // Config hash check: detect query configuration changes that require
            // a full re-bootstrap. If the stored hash doesn't match the current
            // config, all checkpoints are cleared so sources bootstrap from scratch.
            //
            // Checkpoint operations require an active session for transactional
            // backends (e.g., RocksDB). Wrap read/write/clear calls in begin/commit.
            let current_hash = super::compute_config_hash(&self.base.config);

            if let Some(sc) = &session_control {
                sc.begin()
                    .await
                    .context("Failed to begin session for config hash check")?;
            }

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
                    if let Err(e) = checkpoint_store.clear_checkpoints().await {
                        let msg = format!(
                            "Query '{}' failed to clear checkpoints on config change: {e}. \
                             Cannot start with stale checkpoint data from a different config.",
                            self.base.config.id
                        );
                        error!("{msg}");
                        self.base
                            .set_status(ComponentStatus::Error, Some(msg.clone()))
                            .await;
                        return Err(anyhow::anyhow!(msg));
                    }
                    if let Err(e) = clear_persistent_indexes(
                        &self.base.config.id,
                        &element_index,
                        &archive_index,
                        &result_index,
                        &future_queue,
                    )
                    .await
                    {
                        let msg = format!(
                            "Query '{}' failed to clear persistent indexes on config change: {e}",
                            self.base.config.id
                        );
                        error!("{msg}");
                        self.base
                            .set_status(ComponentStatus::Error, Some(msg.clone()))
                            .await;
                        return Err(anyhow::anyhow!(msg));
                    }
                    if let Err(e) = self.clear_output_data().await {
                        let msg = format!(
                            "Query '{}' failed to clear persistent output on config change: {e}",
                            self.base.config.id
                        );
                        error!("{msg}");
                        self.base
                            .set_status(ComponentStatus::Error, Some(msg.clone()))
                            .await;
                        return Err(anyhow::anyhow!(msg));
                    }
                    // Write the hash last. If any clear fails, the next startup
                    // must not trust partially reset state as the new config.
                    checkpoint_store
                        .write_config_hash(current_hash)
                        .await
                        .context("Failed to write new query config hash")?;
                    false
                }
                Ok(None) => {
                    info!(
                        "Query '{}' has no stored config hash, clearing untrusted persistent state and writing hash {current_hash}",
                        self.base.config.id
                    );
                    checkpoint_store
                        .clear_checkpoints()
                        .await
                        .context("Failed to clear checkpoints without a config hash")?;
                    clear_persistent_indexes(
                        &self.base.config.id,
                        &element_index,
                        &archive_index,
                        &result_index,
                        &future_queue,
                    )
                    .await
                    .context("Failed to clear persistent indexes without a config hash")?;
                    self.clear_output_data().await?;
                    checkpoint_store
                        .write_config_hash(current_hash)
                        .await
                        .context("Failed to write initial query config hash")?;
                    false
                }
                Err(e) => {
                    warn!(
                        "Query '{}' failed to read config hash, clearing persistent state and starting fresh: {e}",
                        self.base.config.id
                    );
                    // Cannot trust persistent state if config hash is unreadable —
                    // clear indexes, checkpoints, and outbox before bootstrapping.
                    if let Err(ce) = checkpoint_store.clear_checkpoints().await {
                        let msg = format!(
                            "Query '{}' failed to clear checkpoints on hash read failure: {ce}",
                            self.base.config.id
                        );
                        error!("{msg}");
                        self.base
                            .set_status(ComponentStatus::Error, Some(msg.clone()))
                            .await;
                        return Err(anyhow::anyhow!(msg));
                    }
                    if let Err(ie) = clear_persistent_indexes(
                        &self.base.config.id,
                        &element_index,
                        &archive_index,
                        &result_index,
                        &future_queue,
                    )
                    .await
                    {
                        let msg = format!(
                            "Query '{}' failed to clear persistent indexes on hash read failure: {ie}",
                            self.base.config.id
                        );
                        error!("{msg}");
                        self.base
                            .set_status(ComponentStatus::Error, Some(msg.clone()))
                            .await;
                        return Err(anyhow::anyhow!(msg));
                    }
                    if let Err(oe) = self.clear_output_data().await {
                        let msg = format!(
                            "Query '{}' failed to clear persistent output on hash read failure: {oe}",
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
            persistent_state_matches = config_matches;

            // Only read checkpoints if the config hash matched — otherwise we
            // cleared them above and a full bootstrap will run.
            if config_matches {
                if let Some(sequence) = checkpoint_store
                    .read_result_sequence(&self.base.config.id)
                    .await
                    .with_context(|| {
                        format!(
                            "Query '{}' failed to read its persisted result sequence",
                            self.base.config.id
                        )
                    })?
                {
                    self.establish_output_sequence_floor(sequence).await?;
                }

                match checkpoint_store.read_all_checkpoints().await {
                    Ok(checkpoints) => {
                        for settings in &mut subscription_settings {
                            if let Some(cp) = checkpoints.get(&settings.source_id) {
                                checkpoint_sequences_per_source
                                    .insert(settings.source_id.clone(), cp.sequence);
                                settings.request_position_handle = true;
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
                    Err(e) => {
                        warn!(
                            "Query '{}' failed to read checkpoints, starting fresh: {e}",
                            self.base.config.id
                        );
                    }
                }
            }

            // For persistent queries, always request position handles so sources
            // can track the query's durable progress for min-watermark advancement,
            // even on first run before any checkpoints exist.
            for settings in &mut subscription_settings {
                settings.request_position_handle = true;
            }

            // Commit the startup session used for config hash + checkpoint reads/writes.
            if let Some(sc) = &session_control {
                if let Err(e) = sc.commit().await {
                    warn!(
                        "Query '{}' failed to commit startup session: {e}",
                        self.base.config.id
                    );
                }
            }
        }

        // Hydrate persistent query output state before subscribing to any
        // source. The shared state-store clock and matching legacy reaction
        // checkpoints were already merged by `prepare_output_sequence`.
        let mut sequence_baseline = self.output_state.read().await.as_of_sequence();
        let mut persisted_outbox = Vec::new();
        let mut persisted_results = None;
        let mut persisted_snapshot_sequence = None;
        let mut clear_unversioned_live_results = false;
        let live_results_writer = self.live_results_writer.read().await.clone();
        let durable_reset_sequence = if let Some(store) = self.output_sequence_store.as_ref() {
            read_durable_output_reset(store.as_ref(), &self.base.config.id, self.config_hash)
                .await?
        } else {
            None
        };
        let mut persisted_result_sequence = None;
        if persistent_state_matches {
            persisted_result_sequence = checkpoint_store
                .read_result_sequence(&self.base.config.id)
                .await
                .context("Failed to read persisted query result sequence")?;
            if let Some(sequence) = persisted_result_sequence {
                sequence_baseline = sequence_baseline.max(sequence);
            }
        }

        if let Some(writer) = live_results_writer.as_ref() {
            let writer_sequence = writer
                .read_snapshot_sequence(&self.base.config.id)
                .await
                .context("Failed to read persisted query result snapshot sequence")?;
            let snapshot_sequence =
                [writer_sequence, persisted_result_sequence, durable_reset_sequence]
                    .into_iter()
                    .flatten()
                    .max();
            persisted_snapshot_sequence = Some(snapshot_sequence.unwrap_or(0));
            let mut results = im::HashMap::new();
            if snapshot_sequence.is_some() {
                let rows = writer
                    .read_snapshot(&self.base.config.id)
                    .await
                    .context("Failed to read persisted query result snapshot")?;
                for (row_signature, data) in rows {
                    let value =
                        rmp_serde::from_slice::<serde_json::Value>(&data).with_context(|| {
                            format!(
                                    "Failed to deserialize persisted row {row_signature} for query '{}'",
                                    self.base.config.id
                                )
                        })?;
                    results.insert(row_signature, value);
                }
            } else {
                // Rows without a watermark cannot be tied to this config or
                // sequence. Reconstruct them from a complete outbox below.
                clear_unversioned_live_results = true;
            }
            persisted_results = Some(results);
        }

        let outbox_writer = self.outbox_writer.read().await.clone();
        if let Some(writer) = outbox_writer.as_ref() {
            let latest_sequence = writer
                .read_latest_sequence(&self.base.config.id)
                .await
                .context("Failed to read persisted query outbox sequence")?;
            if let Some(sequence) = latest_sequence {
                sequence_baseline = sequence_baseline.max(sequence);
            }

            let entries = writer
                .read_from(&self.base.config.id, 0)
                .await
                .context("Failed to read persisted query outbox")?;
            persisted_outbox =
                decode_persisted_outbox(&self.base.config.id, latest_sequence, entries)?;
            ensure_persistent_outbox_reaches(
                &self.base.config.id,
                latest_sequence,
                durable_reset_sequence,
                sequence_baseline,
            )?;
        }

        if let (Some(writer), Some(snapshot_sequence)) =
            (live_results_writer.as_ref(), persisted_snapshot_sequence)
        {
            let mut repair_entries = Vec::new();
            if snapshot_sequence < sequence_baseline {
                let required_first = snapshot_sequence.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Persistent result sequence overflow for query '{}'",
                        self.base.config.id
                    )
                })?;
                repair_entries = persisted_outbox
                    .iter()
                    .filter(|entry| entry.sequence >= required_first)
                    .collect();
                let first = repair_entries.first().map(|entry| entry.sequence);
                let last = repair_entries.last().map(|entry| entry.sequence);
                if first != Some(required_first) || last != Some(sequence_baseline) {
                    anyhow::bail!(
                        "Persistent live results for query '{}' are only certified through sequence {}, but the outbox cannot repair the complete range {}..={}",
                        self.base.config.id,
                        snapshot_sequence,
                        required_first,
                        sequence_baseline
                    );
                }
            }
            if clear_unversioned_live_results {
                writer
                    .clear(&self.base.config.id)
                    .await
                    .context("Failed to clear unversioned persistent live results")?;
            }
            for entry in repair_entries {
                persist_live_result(writer.as_ref(), &self.base.config.id, entry.as_ref())
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to repair persistent live results for query '{}' at sequence {}",
                            self.base.config.id, entry.sequence
                        )
                    })?;
            }
        }

        checkpoint_store
            .write_result_sequence(&self.base.config.id, sequence_baseline)
            .await
            .context("Failed to establish persisted query result sequence")?;
        self.establish_output_sequence_floor(sequence_baseline)
            .await?;

        {
            let mut state = self.output_state.write().await;
            let current_sequence = state.as_of_sequence();
            if current_sequence < sequence_baseline {
                state
                    .rebase_sequence(sequence_baseline - current_sequence)
                    .map_err(|error| anyhow::anyhow!("Failed to restore query output: {error}"))?;
            }
            sequence_baseline = state.as_of_sequence();
            if state.results_len() == 0 {
                if let Some(results) = persisted_results {
                    state.restore_results(results);
                    let snapshot_sequence = persisted_snapshot_sequence.unwrap_or(0);
                    for entry in persisted_outbox
                        .iter()
                        .filter(|entry| entry.sequence > snapshot_sequence)
                    {
                        state.apply_diffs(&entry.results);
                    }
                }
            }
            if state.outbox_len() == 0 {
                state.restore_outbox(persisted_outbox);
            }
            self.output_metrics.update_outbox(
                state.outbox_len(),
                state.outbox_earliest_seq().unwrap_or(0),
                state.as_of_sequence(),
            );
        }
        if sequence_baseline > 0 {
            info!(
                "Query '{}' restored output sequence baseline to {}",
                self.base.config.id, sequence_baseline
            );
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
        let mut reset_output_baseline = None;

        'subscribe_loop: loop {
            // On AutoReset retry: clear resume positions so sources bootstrap from scratch.
            if auto_reset_retry {
                info!(
                    "Query '{}' auto-reset: clearing resume positions and re-subscribing all sources",
                    self.base.config.id
                );
                for (_, _, settings) in &mut sources_to_subscribe {
                    settings.resume_from = None;
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

                                            // Clear all persistent state. If clearing fails,
                                            // abort startup rather than mixing stale state with
                                            // a fresh bootstrap.
                                            if has_persistent_backend {
                                                // Begin a session for the clear operations
                                                if let Some(sc) = &session_control {
                                                    if let Err(e) = sc.begin().await {
                                                        let msg = format!(
                                                            "Query '{}' auto-reset failed: could not begin session: {e}",
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
                                                            "AutoReset aborted: failed to begin session for clearing: {e}",
                                                        ));
                                                    }
                                                }

                                                if let Err(ie) = clear_persistent_indexes(
                                                    &self.base.config.id,
                                                    &element_index,
                                                    &archive_index,
                                                    &result_index,
                                                    &future_queue,
                                                )
                                                .await
                                                {
                                                    if let Some(sc) = &session_control {
                                                        let _ = sc.rollback();
                                                    }
                                                    let msg = format!(
                                                        "Query '{}' auto-reset failed: could not clear persistent indexes: {ie}",
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
                                                        "AutoReset aborted: failed to clear persistent indexes: {ie}",
                                                    ));
                                                }
                                                if let Err(ce) =
                                                    checkpoint_store.clear_checkpoints().await
                                                {
                                                    // Rollback on failure
                                                    if let Some(sc) = &session_control {
                                                        let _ = sc.rollback();
                                                    }
                                                    let msg = format!(
                                                        "Query '{}' auto-reset failed: could not clear checkpoints: {ce}",
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
                                                        "AutoReset aborted: failed to clear checkpoints: {ce}",
                                                    ));
                                                }
                                                reset_output_baseline = Some(
                                                    self.output_state.read().await.clone_results(),
                                                );
                                                if let Err(oe) = self.clear_output_data().await {
                                                    if let Some(sc) = &session_control {
                                                        let _ = sc.rollback();
                                                    }
                                                    let msg = format!(
                                                        "Query '{}' auto-reset failed: could not clear persistent output: {oe}",
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
                                                        "AutoReset aborted: failed to clear persistent output: {oe}",
                                                    ));
                                                }
                                                // Write current config hash so next normal restart resumes correctly
                                                let current_hash =
                                                    super::compute_config_hash(&self.base.config);
                                                if let Err(he) = checkpoint_store
                                                    .write_config_hash(current_hash)
                                                    .await
                                                {
                                                    warn!(
                                                        "Query '{}' failed to write config hash during auto-reset: {he}",
                                                        self.base.config.id
                                                    );
                                                }
                                                let output_sequence =
                                                    self.output_state.read().await.as_of_sequence();
                                                if let Err(se) = checkpoint_store
                                                    .write_result_sequence(
                                                        &self.base.config.id,
                                                        output_sequence,
                                                    )
                                                    .await
                                                {
                                                    if let Some(sc) = &session_control {
                                                        let _ = sc.rollback();
                                                    }
                                                    let msg = format!(
                                                        "Query '{}' auto-reset failed: could not establish result sequence: {se}",
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
                                                        "AutoReset aborted: failed to establish result sequence: {se}",
                                                    ));
                                                }

                                                // Commit the clearing session
                                                if let Some(sc) = &session_control {
                                                    if let Err(e) = sc.commit().await {
                                                        warn!(
                                                            "Query '{}' failed to commit auto-reset session: {e}",
                                                            self.base.config.id
                                                        );
                                                    }
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
        *self.subscribed_source_ids.write().await = sources_to_subscribe
            .iter()
            .map(|(id, _, _)| id.clone())
            .collect();

        // Wrap continuous_query in Arc for sharing across tasks
        let continuous_query = Arc::new(continuous_query);

        // Gate that blocks the streaming event processor until bootstrap completes.
        // Events buffer safely in the priority queue during bootstrap.
        let bootstrap_gate = Arc::new(Notify::new());

        // NEW: Handle bootstrap channels
        if !bootstrap_channels.is_empty() {
            let complete_bootstrap = bootstrap_channels.len() == sources_to_subscribe.len();
            if auto_reset_retry && !complete_bootstrap {
                let message = format!(
                    "Query '{}' cannot complete AutoReset because only {}/{} sources supplied bootstrap snapshots",
                    self.base.config.id,
                    bootstrap_channels.len(),
                    sources_to_subscribe.len()
                );
                self.base
                    .set_status(ComponentStatus::Error, Some(message.clone()))
                    .await;
                return Err(anyhow::anyhow!(message));
            }
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
            let bootstrap_output_state = self.output_state.clone();
            let pre_bootstrap_results = reset_output_baseline
                .take()
                .unwrap_or(self.output_state.read().await.clone_results());
            if complete_bootstrap {
                self.output_state
                    .write()
                    .await
                    .clear_results_preserving_outbox();
            }

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
                let output_state_clone = bootstrap_output_state.clone();

                let span = tracing::info_span!(
                    "query_bootstrap",
                    instance_id = %instance_id_clone,
                    component_id = %query_id,
                    component_type = "query"
                );
                let handle: tokio::task::JoinHandle<(String, anyhow::Result<Option<crate::bootstrap::BootstrapResult>>)> = tokio::spawn(
                    async move {
                        let mut count = 0u64;
                        let mut evaluation_error = None;

                        while let Some(bootstrap_event) = bootstrap_rx.recv().await {
                            count += 1;

                            // Process bootstrap change through ContinuousQuery
                            match continuous_query_ref
                                .process_source_change(bootstrap_event.change)
                                .await
                            {
                                Ok(results) => {
                                    if !results.is_empty() {
                                        debug!(
                                            "[BOOTSTRAP] Query '{}' received {} results from bootstrap event {}",
                                            query_id_clone, results.len(), count
                                        );

                                        // Convert results to ResultDiffs and apply to output state.
                                        // During bootstrap, we only update the result set (no outbox
                                        // push, no sequence increment, no dispatch to reactions).
                                        let diffs: Vec<ResultDiff> = results
                                            .iter()
                                            .map(|ctx| match ctx {
                                                QueryPartEvaluationContext::Adding { after, row_signature } => {
                                                    ResultDiff::Add {
                                                        data: convert_query_variables_to_json(after),
                                                        row_signature: *row_signature,
                                                    }
                                                }
                                                QueryPartEvaluationContext::Removing { before, row_signature } => {
                                                    ResultDiff::Delete {
                                                        data: convert_query_variables_to_json(before),
                                                        row_signature: *row_signature,
                                                    }
                                                }
                                                QueryPartEvaluationContext::Updating { before, after, row_signature } => {
                                                    let after_json = convert_query_variables_to_json(after);
                                                    ResultDiff::Update {
                                                        data: after_json.clone(),
                                                        before: convert_query_variables_to_json(before),
                                                        after: after_json,
                                                        grouping_keys: None,
                                                        row_signature: *row_signature,
                                                    }
                                                }
                                                QueryPartEvaluationContext::Aggregation { before, after, row_signature, .. } => {
                                                    ResultDiff::Aggregation {
                                                        before: before.as_ref().map(convert_query_variables_to_json),
                                                        after: convert_query_variables_to_json(after),
                                                        row_signature: *row_signature,
                                                    }
                                                }
                                                QueryPartEvaluationContext::Noop => ResultDiff::Noop,
                                            })
                                            .collect();

                                        let mut state = output_state_clone.write().await;
                                        state.apply_diffs(&diffs);
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "[BOOTSTRAP] Query '{query_id_clone}' failed to process bootstrap event from source '{source_id_clone}': {e}"
                                    );
                                    evaluation_error = Some(anyhow::anyhow!(
                                        "Query '{query_id_clone}' failed to process bootstrap event {count} from source '{source_id_clone}': {e}"
                                    ));
                                    break;
                                }
                            }
                        }

                        if let Some(error) = evaluation_error {
                            return (source_id_clone, Err(error));
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
                let output_state_for_supervisor = self.output_state.clone();
                let pre_bootstrap_results_for_supervisor = pre_bootstrap_results.clone();
                let outbox_writer_for_supervisor = self.outbox_writer.read().await.clone();
                let live_results_writer_for_supervisor =
                    self.live_results_writer.read().await.clone();
                let output_sequence_store_for_supervisor = self.output_sequence_store.clone();
                let outbox_capacity_for_supervisor =
                    self.output_state.read().await.outbox_capacity();
                let output_metrics_for_supervisor = self.output_metrics.clone();

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

                        // Make the bootstrap output durable before committing source
                        // handover positions. A crash may safely repeat bootstrap, but
                        // must never resume past a snapshot that reactions cannot replay.
                        if let Err(e) = publish_bootstrap_output(
                            &query_id_clone,
                            &output_state_for_supervisor,
                            &pre_bootstrap_results_for_supervisor,
                            &base_dispatchers_clone,
                            &outbox_writer_for_supervisor,
                            &live_results_writer_for_supervisor,
                            &checkpoint_store_for_supervisor,
                            &output_sequence_store_for_supervisor,
                            outbox_capacity_for_supervisor,
                            &output_metrics_for_supervisor,
                        )
                        .await
                        {
                            error!(
                                "[BOOTSTRAP] Query '{query_id_clone}' failed to publish durable bootstrap output: {e:#}"
                            );
                            reporter_clone
                                .set_status(
                                    ComponentStatus::Error,
                                    Some(format!(
                                        "Bootstrap output persistence failed: {e:#}"
                                    )),
                                )
                                .await;
                            bootstrap_gate_clone.notify_one();
                            return;
                        }

                        // Persist recovery checkpoints before opening the gate.
                        // This avoids a redundant re-bootstrap after successful output
                        // publication while preserving crash-safe replay.
                        if !handover_positions.is_empty() {
                            let session_ok = if let Some(sc) = &session_control_for_supervisor {
                                match sc.begin().await {
                                    Ok(()) => true,
                                    Err(e) => {
                                        warn!(
                                            "[BOOTSTRAP] Query '{query_id_clone}' failed to begin session \
                                             for handover persistence: {e}; checkpoints will not be \
                                             persisted until the first streaming event"
                                        );
                                        false
                                    }
                                }
                            } else {
                                true // no session control needed (e.g. in-memory store)
                            };

                            if session_ok {
                                for (source_id, position) in &handover_positions {
                                    if let Err(e) = checkpoint_store_for_supervisor
                                        .stage_checkpoint(source_id, 0, position.as_ref())
                                        .await
                                    {
                                        warn!(
                                            "[BOOTSTRAP] Query '{query_id_clone}' failed to persist \
                                             handover checkpoint for '{source_id}': {e}"
                                        );
                                    }
                                }
                                if let Some(sc) = &session_control_for_supervisor {
                                    if let Err(e) = sc.commit().await {
                                        warn!(
                                            "[BOOTSTRAP] Query '{query_id_clone}' failed to commit \
                                             handover checkpoints: {e}"
                                        );
                                    }
                                }
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
            // No bootstrap needed — open the gate immediately
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
        let checkpoint_store_for_dispatch: Option<Arc<dyn CheckpointStore>> =
            Some(checkpoint_store.clone());
        let base_dispatchers = self.base.dispatchers.clone();
        let query_id = self.base.config.id.clone();
        let output_state = self.output_state.clone();
        let task_handle_clone = self.base.task_handle.clone();
        let priority_queue = self.priority_queue.clone();
        let instance_id = self.instance_id.clone();
        let reporter_for_processor = self.base.status_handle();
        let fq_source_for_processor = Arc::clone(&future_queue_source);
        let position_handles_for_processor = position_handles;
        let outbox_writer_for_processor = self.outbox_writer.read().await.clone();
        let live_results_writer_for_processor = self.live_results_writer.read().await.clone();
        let output_sequence_store_for_processor = self.output_sequence_store.clone();
        let outbox_capacity_for_processor = self.output_state.read().await.outbox_capacity();
        let output_metrics_for_processor = self.output_metrics.clone();
        let source_ids_for_processor: Vec<String> = self
            .base
            .config
            .sources
            .iter()
            .map(|s| s.source_id.clone())
            .collect();

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

                loop {
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
                                    // Drain all due futures atomically within sessions
                                    loop {
                                        match continuous_query_for_processor.process_due_futures().await {
                                            Ok(Some(due_result)) => {
                                                if !due_result.results.is_empty() {
                                                    let profiling = crate::profiling::ProfilingMetadata::new();
                                                    dispatch_query_results(
                                                        &due_result.results,
                                                        &due_result.source_id,
                                                        &query_id,
                                                        &output_state,
                                                        &base_dispatchers,
                                                        &outbox_writer_for_processor,
                                                        &live_results_writer_for_processor,
                                                        &checkpoint_store_for_dispatch,
                                                        &output_sequence_store_for_processor,
                                                        outbox_capacity_for_processor,
                                                        profiling,
                                                        &output_metrics_for_processor,
                                                    )
                                                    .await;
                                                }
                                            }
                                            Ok(None) => break,
                                            Err(e) => {
                                                error!("Query '{query_id}' failed to process due futures: {e}");
                                                break;
                                            }
                                        }
                                    }
                                    continue;
                                }
                                SourceEvent::Change(source_change) => {
                                    let mut profiling =
                                        profiling_opt.unwrap_or_else(crate::profiling::ProfilingMetadata::new);
                                    profiling.query_receive_ns = Some(crate::profiling::timestamp_ns());
                                    profiling.query_core_call_ns = Some(crate::profiling::timestamp_ns());

                                    // Stage checkpoint inside the session via pre-commit hook.
                                    // This ensures checkpoint persistence is atomic with index updates.
                                    let cp_store = checkpoint_store_for_processor.clone();
                                    let cp_source_id = source_id.clone();
                                    let cp_position = source_position.clone();
                                    let hook = move || {
                                        async move {
                                            if let Some(seq) = sequence {
                                                // Enforce position size limit at checkpoint time:
                                                // oversized positions are skipped to preserve the
                                                // last known good position in the store.
                                                let pos_ref = match &cp_position {
                                                    Some(p) if p.len() <= crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES => Some(p),
                                                    _ => None,
                                                };
                                                cp_store
                                                    .stage_checkpoint(&cp_source_id, seq, pos_ref)
                                                    .await?;
                                            }
                                            Ok(())
                                        }
                                    };

                                    match continuous_query_for_processor
                                        .process_source_change_with_hook(source_change, hook)
                                        .await
                                    {
                                        Ok(results) => {
                                            profiling.query_core_return_ns = Some(crate::profiling::timestamp_ns());

                                            // Advance dedup and notify source on successful commit
                                            if let Some(seq) = sequence {
                                                dedup.advance(&source_id, seq);

                                                if let Some(handle) = position_handles_for_processor.get(&source_id) {
                                                    handle.store(seq, std::sync::atomic::Ordering::Release);
                                                }
                                            }

                                            if !results.is_empty() {
                                                profiling.query_send_ns = Some(crate::profiling::timestamp_ns());
                                                dispatch_query_results(
                                                    &results,
                                                    &source_id,
                                                    &query_id,
                                                    &output_state,
                                                    &base_dispatchers,
                                                    &outbox_writer_for_processor,
                                                    &live_results_writer_for_processor,
                                                    &checkpoint_store_for_dispatch,
                                                    &output_sequence_store_for_processor,
                                                    outbox_capacity_for_processor,
                                                    profiling,
                                                    &output_metrics_for_processor,
                                                )
                                                .await;
                                            }
                                        }
                                        Err(e) => {
                                            error!("Query '{query_id}' failed to process source change: {e}");
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

        // Abort bootstrap tasks and supervisor
        let bootstrap_aborts: Vec<_> = {
            let mut handles = self.bootstrap_abort_handles.write().await;
            handles.drain(..).collect()
        };
        for handle in bootstrap_aborts {
            handle.abort();
        }

        // Drain and abort source subscription forwarders so they don't leak across restarts
        let subscription_handles: Vec<_> = {
            let mut tasks = self.subscription_tasks.write().await;
            tasks.drain(..).collect()
        };

        for handle in subscription_handles {
            handle.abort();
            let _ = handle.await;
        }

        // Stop the FutureQueueSource polling task
        if let Some(fq) = self.future_queue_source.write().await.take() {
            fq.stop().await;
        }

        // Release position handles so sources can advance their min-watermark.
        // Each subscribed source may hold a position handle for this query.
        {
            let source_ids = self.subscribed_source_ids.read().await;
            for source_id in source_ids.iter() {
                if let Some(source) = self.source_manager.get_source_instance(source_id).await {
                    source.remove_position_handle(&self.base.config.id).await;
                    debug!(
                        "Query '{}' released position handle for source '{}'",
                        self.base.config.id, source_id
                    );
                }
            }
        }
        // Clear tracked source IDs
        self.subscribed_source_ids.write().await.clear();

        // Use QueryBase common stop behavior to finish shutting down the processor task
        self.base.stop_common().await?;

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

    async fn ensure_output_sequence_at_least(&self, minimum: u64) -> Result<()> {
        self.establish_output_sequence_floor(minimum).await
    }

    async fn restore_output_sequence_baseline(&self) -> Result<()> {
        self.restore_output_sequence().await
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
    /// Shared state store used for restart-stable query output sequences.
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
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
            state_store: Arc::new(RwLock::new(None)),
            default_recovery_policy,
            label_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Inject the shared state store before queries are provisioned.
    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
    }

    async fn prepare_output_sequence(&self, query_id: &str, query: &Arc<dyn Query>) -> Result<()> {
        query.restore_output_sequence_baseline().await?;

        let Some(store) = self.state_store.read().await.clone() else {
            return Ok(());
        };
        let config_hash = crate::queries::compute_config_hash(query.get_config());
        let reaction_ids = {
            let graph = self.graph.read().await;
            graph
                .get_dependents(query_id)
                .into_iter()
                .filter(|node| node.kind == ComponentKind::Reaction)
                .map(|node| node.id.clone())
                .collect::<Vec<_>>()
        };

        let mut sequence_floor = 0;
        for reaction_id in reaction_ids {
            if let Some(checkpoint) = crate::reactions::checkpoint::read_checkpoint(
                store.as_ref(),
                &reaction_id,
                query_id,
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to read restart checkpoint for reaction '{reaction_id}', query '{query_id}'"
                )
            })?
            {
                if checkpoint.config_hash == config_hash {
                    sequence_floor = sequence_floor.max(checkpoint.sequence);
                }
            }
        }

        query
            .ensure_output_sequence_at_least(sequence_floor)
            .await
            .with_context(|| format!("Failed to prepare result sequence for query '{query_id}'"))
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
            self.state_store.read().await.clone(),
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

        self.prepare_output_sequence(&id, &query).await?;
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
        let query_config = {
            let graph = self.graph.read().await;
            graph
                .get_runtime::<Arc<dyn Query>>(&id)
                .map(|q| q.get_config().clone())
        };

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

        if let Some(store) = self.state_store.read().await.as_ref() {
            store
                .clear_store(&format!("{QUERY_OUTPUT_OUTBOX_STORE_PREFIX}{id}"))
                .await
                .with_context(|| {
                    format!("Failed to clear durable outbox for removed query '{id}'")
                })?;
            store
                .clear_store(&format!("{QUERY_OUTPUT_LIVE_RESULTS_STORE_PREFIX}{id}"))
                .await
                .with_context(|| {
                    format!("Failed to clear durable live results for removed query '{id}'")
                })?;
            store
                .delete(
                    QUERY_OUTPUT_SEQUENCE_STORE_ID,
                    &format!("{QUERY_OUTPUT_RESET_KEY_PREFIX}{id}"),
                )
                .await
                .with_context(|| {
                    format!("Failed to clear durable output reset for removed query '{id}'")
                })?;
            store
                .delete(QUERY_OUTPUT_SEQUENCE_STORE_ID, &id)
                .await
                .with_context(|| {
                    format!("Failed to clear durable output sequence for removed query '{id}'")
                })?;
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
                            // Wrap clearing in a session for transactional backends
                            if let Err(e) = created.set.session_control.begin().await {
                                warn!(
                                    "Query '{id}' failed to begin session for removal cleanup: {e}"
                                );
                            } else {
                                if let Err(e) = clear_persistent_indexes(
                                    &id,
                                    &Some(created.set.element_index),
                                    &Some(created.set.archive_index),
                                    &Some(created.set.result_index),
                                    &Some(created.set.future_queue),
                                )
                                .await
                                {
                                    warn!(
                                        "Query '{id}' failed to clear persistent indexes on removal: {e}"
                                    );
                                }

                                if let Some(checkpoint_store) = created.checkpoint_store {
                                    if let Err(e) = checkpoint_store.clear_checkpoints().await {
                                        warn!(
                                            "Query '{id}' failed to clear checkpoints on removal: {e}"
                                        );
                                    }
                                }
                                if let Some(outbox_writer) = created.outbox_writer {
                                    if let Err(e) = outbox_writer.clear(&id).await {
                                        warn!(
                                            "Query '{id}' failed to clear persistent outbox on removal: {e}"
                                        );
                                    }
                                }
                                if let Some(live_results_writer) = created.live_results_writer {
                                    if let Err(e) = live_results_writer.clear(&id).await {
                                        warn!(
                                            "Query '{id}' failed to clear persistent live results on removal: {e}"
                                        );
                                    }
                                }

                                if let Err(e) = created.set.session_control.commit().await {
                                    warn!(
                                        "Query '{id}' failed to commit removal cleanup session: {e}"
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
                self.prepare_output_sequence(&id, &query).await?;

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
