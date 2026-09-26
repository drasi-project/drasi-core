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

//! Characterization tests for the fixed Source -> Continuous Query -> Reaction pipeline.
//!
//! These tests pin the computation runtime's transaction and output boundaries:
//! indexes, source progress, and durable output commit together before dispatch.
//! The historical test names are retained for the pinned runtime-parity corpus.

use std::{
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    ops::Bound,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::aggregation::ValueAccumulator,
        variable_value::VariableValue,
    },
    interface::{
        AccumulatorIndex, CheckpointStore, CreatedIndexes, ElementArchiveIndex, ElementIndex,
        ElementStream, FutureElementRef, FutureQueue, IndexBackendPlugin, IndexError, IndexSet,
        LazySortedSetStore, LiveResultsWriter, OutboxWriter, PushType, ResultIndex, ResultKey,
        ResultOwner, ResultSequence, ResultSequenceCounter, RowMutation, SessionControl,
        SourceCheckpoint,
    },
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp,
        QueryJoin, SourceChange, TimestampRange,
    },
    path_solver::match_path::MatchPath,
};
use futures::FutureExt;
use ordered_float::OrderedFloat;
use serde_json::json;
use tokio::sync::mpsc;

use super::*;
use crate::{
    channels::{
        ChangeReceiver, ComponentStatus, QueryResult, ResultDiff, SourceEvent, SourceEventWrapper,
        SubscriptionResponse,
    },
    computation::v1::{progress_key, EnvelopeCodec, QueryChangeCodec},
    config::{QueryLanguage, SourceSubscriptionConfig},
    indexes::config::StorageBackendRef,
    profiling::ProfilingMetadata,
    sources::{Source, SourceBase, SourceBaseParams},
};

const SOURCE_ID: &str = "pipeline-source";
const QUERY_ID: &str = "pipeline-query";

#[derive(Debug, Clone, PartialEq, Eq)]
enum TraceEvent {
    SessionBegin,
    CheckpointStaged {
        source_id: String,
        sequence: u64,
        source_position: Option<Bytes>,
    },
    SessionCommit,
    SessionRollback,
    OutboxAppendAttempt {
        query_id: String,
        sequence: u64,
    },
    LiveResultsApplyAttempt {
        query_id: String,
        mutation_count: usize,
    },
    ResultSequenceWritten {
        query_id: String,
        sequence: u64,
    },
    ResultSequenceStaged {
        query_id: String,
        sequence: u64,
    },
    ReactionDispatch {
        sequence: u64,
    },
}

#[derive(Clone, Default)]
struct EventTrace {
    events: Arc<Mutex<Vec<TraceEvent>>>,
}

impl EventTrace {
    fn record(&self, event: TraceEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn clear(&self) {
        self.events.lock().unwrap().clear();
    }

    fn snapshot(&self) -> Vec<TraceEvent> {
        self.events.lock().unwrap().clone()
    }
}

type StoredRows = HashMap<String, BTreeMap<u64, Vec<u8>>>;
type SortedSets = HashMap<u64, BTreeMap<OrderedFloat<f64>, isize>>;

#[derive(Clone, Default)]
struct TransactionData {
    elements: HashMap<ElementReference, (Arc<Element>, Vec<usize>)>,
    accumulators: HashMap<u64, ValueAccumulator>,
    sorted_sets: SortedSets,
    index_sequence: ResultSequence,
    futures: Vec<(usize, FutureElementRef)>,
    checkpoints: HashMap<String, SourceCheckpoint>,
    result_sequences: HashMap<String, u64>,
    outbox: StoredRows,
    live_results: StoredRows,
}

#[derive(Default)]
struct TransactionState {
    pending: Option<TransactionData>,
    committed: TransactionData,
    config_hash: Option<u64>,
    output_generations: HashMap<String, u64>,
}

impl TransactionState {
    fn read(&self) -> &TransactionData {
        self.pending.as_ref().unwrap_or(&self.committed)
    }

    fn stage(&mut self) -> Result<&mut TransactionData, IndexError> {
        self.pending
            .as_mut()
            .ok_or_else(|| IndexError::other(std::io::Error::other("write outside a test session")))
    }

    fn clearable(&mut self) -> &mut TransactionData {
        match &mut self.pending {
            Some(pending) => pending,
            None => &mut self.committed,
        }
    }
}

struct RecordingSessionControl {
    state: Arc<Mutex<TransactionState>>,
    trace: EventTrace,
    output_gate: Mutex<Option<Arc<CommitGate>>>,
}

#[derive(Default)]
struct CommitGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl SessionControl for RecordingSessionControl {
    async fn begin(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        if state.pending.is_some() {
            return Err(IndexError::other(std::io::Error::other(
                "test session already active",
            )));
        }
        state.pending = Some(state.committed.clone());
        self.trace.record(TraceEvent::SessionBegin);
        Ok(())
    }

    async fn commit(&self) -> Result<(), IndexError> {
        let gate = {
            let state = self.state.lock().unwrap();
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.outbox != state.committed.outbox)
            {
                self.output_gate.lock().unwrap().take()
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        let mut state = self.state.lock().unwrap();
        state.committed = state.pending.take().ok_or_else(|| {
            IndexError::other(std::io::Error::other("test session is not active"))
        })?;
        self.trace.record(TraceEvent::SessionCommit);
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        state.pending = None;
        self.trace.record(TraceEvent::SessionRollback);
        Ok(())
    }
}

// The suite exercises node queries without archive or synthetic joins. All
// mutable index state still shares the output/checkpoint transaction so a failed
// output write cannot leave an input applied when the recovery test replays it.
struct TransactionalIndexes {
    state: Arc<Mutex<TransactionState>>,
}

#[async_trait]
impl ElementIndex for TransactionalIndexes {
    async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .read()
            .elements
            .get(reference)
            .map(|(element, _)| element.clone()))
    }

    async fn set_element(&self, element: &Element, slots: &Vec<usize>) -> Result<(), IndexError> {
        self.state.lock().unwrap().stage()?.elements.insert(
            element.get_reference().clone(),
            (Arc::new(element.clone()), slots.clone()),
        );
        Ok(())
    }

    async fn delete_element(&self, reference: &ElementReference) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .stage()?
            .elements
            .remove(reference);
        Ok(())
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .read()
            .elements
            .get(reference)
            .filter(|(_, slots)| slots.contains(&slot))
            .map(|(element, _)| element.clone()))
    }

    async fn get_slot_elements_by_inbound(
        &self,
        _slot: usize,
        _reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn get_slot_elements_by_outbound(
        &self,
        _slot: usize,
        _reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.state.lock().unwrap().clearable().elements.clear();
        Ok(())
    }

    async fn set_joins(&self, _match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        assert!(
            joins.is_empty(),
            "the fixture does not implement synthetic joins"
        );
    }
}

#[async_trait]
impl ElementArchiveIndex for TransactionalIndexes {
    async fn get_element_as_at(
        &self,
        _reference: &ElementReference,
        _time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn get_element_versions(
        &self,
        _reference: &ElementReference,
        _range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn clear(&self) -> Result<(), IndexError> {
        Ok(())
    }
}

fn accumulator_key(key: &ResultKey, owner: &ResultOwner) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    owner.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

#[async_trait]
impl AccumulatorIndex for TransactionalIndexes {
    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .read()
            .accumulators
            .get(&accumulator_key(key, owner))
            .cloned())
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        let values = &mut state.stage()?.accumulators;
        let key = accumulator_key(&key, &owner);
        match value {
            Some(value) => {
                values.insert(key, value);
            }
            None => {
                values.remove(&key);
            }
        }
        Ok(())
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        let data = state.clearable();
        data.accumulators.clear();
        data.sorted_sets.clear();
        Ok(())
    }
}

#[async_trait]
impl LazySortedSetStore for TransactionalIndexes {
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        let state = self.state.lock().unwrap();
        Ok(state.read().sorted_sets.get(&set_id).and_then(|set| {
            let next = match value {
                Some(value) => set.range((Bound::Excluded(value), Bound::Unbounded)).next(),
                None => set.first_key_value(),
            };
            next.map(|(value, count)| (*value, *count))
        }))
    }

    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .read()
            .sorted_sets
            .get(&set_id)
            .and_then(|set| set.get(&value))
            .copied()
            .unwrap_or(0))
    }

    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        let set = state.stage()?.sorted_sets.entry(set_id).or_default();
        let count = set.get(&value).copied().unwrap_or(0) + delta;
        if count < 0 {
            return Err(IndexError::CorruptedData);
        }
        if count == 0 {
            set.remove(&value);
        } else {
            set.insert(value, count);
        }
        Ok(())
    }
}

#[async_trait]
impl ResultSequenceCounter for TransactionalIndexes {
    async fn apply_sequence(
        &self,
        sequence: u64,
        source_change_id: &str,
    ) -> Result<(), IndexError> {
        self.state.lock().unwrap().stage()?.index_sequence = ResultSequence {
            sequence,
            source_change_id: source_change_id.into(),
        };
        Ok(())
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        Ok(self.state.lock().unwrap().read().index_sequence.clone())
    }
}

impl ResultIndex for TransactionalIndexes {}

#[async_trait]
impl FutureQueue for TransactionalIndexes {
    async fn push(
        &self,
        push_type: PushType,
        position: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let mut state = self.state.lock().unwrap();
        let futures = &mut state.stage()?.futures;
        let matches = |(slot, future): &(usize, FutureElementRef)| {
            *slot == position && future.group_signature == group_signature
        };
        if push_type == PushType::IfNotExists && futures.iter().any(matches) {
            return Ok(false);
        }
        if push_type == PushType::Overwrite {
            futures.retain(|item| !matches(item));
        }
        let entry = (
            position,
            FutureElementRef {
                element_ref: element_ref.clone(),
                original_time,
                due_time,
                group_signature,
            },
        );
        if !futures.contains(&entry) {
            futures.push(entry);
        }
        Ok(true)
    }

    async fn remove(&self, position: usize, group_signature: u64) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .stage()?
            .futures
            .retain(|(slot, future)| {
                *slot != position || future.group_signature != group_signature
            });
        Ok(())
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let mut state = self.state.lock().unwrap();
        let futures = &mut state.stage()?.futures;
        let next = futures
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, future))| future.due_time)
            .map(|(index, _)| index);
        Ok(next.map(|index| futures.remove(index).1))
    }

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .read()
            .futures
            .iter()
            .map(|(_, future)| future.due_time)
            .min())
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.state.lock().unwrap().clearable().futures.clear();
        Ok(())
    }
}

struct TransactionalCheckpointStore {
    state: Arc<Mutex<TransactionState>>,
    trace: EventTrace,
}

#[async_trait]
impl CheckpointStore for TransactionalCheckpointStore {
    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let checkpoint = SourceCheckpoint::new(sequence, source_position.cloned());
        let mut state = self.state.lock().unwrap();
        state
            .stage()?
            .checkpoints
            .insert(source_id.to_string(), checkpoint.clone());
        self.trace.record(TraceEvent::CheckpointStaged {
            source_id: source_id.to_string(),
            sequence,
            source_position: checkpoint.source_position,
        });
        Ok(())
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed
            .checkpoints
            .get(source_id)
            .cloned())
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        Ok(self.state.lock().unwrap().committed.checkpoints.clone())
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        state.committed.checkpoints.clear();
        if let Some(pending) = &mut state.pending {
            pending.checkpoints.clear();
        }
        state.config_hash = None;
        Ok(())
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        self.state.lock().unwrap().config_hash = Some(hash);
        Ok(())
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        Ok(self.state.lock().unwrap().config_hash)
    }

    async fn stage_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .stage()?
            .result_sequences
            .insert(query_id.to_string(), sequence);
        self.trace.record(TraceEvent::ResultSequenceStaged {
            query_id: query_id.to_string(),
            sequence,
        });
        Ok(())
    }

    async fn write_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        state
            .committed
            .result_sequences
            .insert(query_id.to_string(), sequence);
        if let Some(pending) = &mut state.pending {
            pending
                .result_sequences
                .insert(query_id.to_string(), sequence);
        }
        self.trace.record(TraceEvent::ResultSequenceWritten {
            query_id: query_id.to_string(),
            sequence,
        });
        Ok(())
    }

    async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed
            .result_sequences
            .get(query_id)
            .copied())
    }

    async fn write_output_generation(
        &self,
        query_id: &str,
        generation: u64,
    ) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .output_generations
            .insert(query_id.to_string(), generation);
        Ok(())
    }

    async fn read_output_generation(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .output_generations
            .get(query_id)
            .copied())
    }
}

struct RecordingOutputStore {
    fail_live_results: AtomicBool,
    trace: EventTrace,
    state: Arc<Mutex<TransactionState>>,
}

impl RecordingOutputStore {
    fn new(
        fail_live_results: bool,
        trace: EventTrace,
        state: Arc<Mutex<TransactionState>>,
    ) -> Self {
        Self {
            fail_live_results: AtomicBool::new(fail_live_results),
            trace,
            state,
        }
    }

    fn injected_failure() -> IndexError {
        IndexError::other(std::io::Error::other("injected output persistence failure"))
    }
}

#[async_trait]
impl OutboxWriter for RecordingOutputStore {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        self.trace.record(TraceEvent::OutboxAppendAttempt {
            query_id: query_id.to_string(),
            sequence,
        });
        self.state
            .lock()
            .unwrap()
            .stage()?
            .outbox
            .entry(query_id.to_string())
            .or_default()
            .insert(sequence, data.to_vec());
        Ok(())
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed
            .outbox
            .get(query_id)
            .map(|entries| {
                entries
                    .range((after_sequence.saturating_add(1))..)
                    .map(|(sequence, data)| (*sequence, data.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed
            .outbox
            .get(query_id)
            .and_then(|entries| entries.last_key_value().map(|(sequence, _)| *sequence)))
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .clearable()
            .outbox
            .remove(query_id);
        Ok(())
    }

    async fn trim_before(&self, query_id: &str, retain_from: u64) -> Result<usize, IndexError> {
        let mut state = self.state.lock().unwrap();
        let Some(entries) = state.stage()?.outbox.get_mut(query_id) else {
            return Ok(0);
        };
        let previous_len = entries.len();
        entries.retain(|sequence, _| *sequence >= retain_from);
        Ok(previous_len - entries.len())
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        let mut state = self.state.lock().unwrap();
        let Some(entries) = state.stage()?.outbox.get_mut(query_id) else {
            return Ok(0);
        };
        let mut removed = 0;
        while entries.len() > capacity {
            let sequence = *entries.first_key_value().unwrap().0;
            entries.remove(&sequence);
            removed += 1;
        }
        Ok(removed)
    }
}

#[async_trait]
impl LiveResultsWriter for RecordingOutputStore {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        self.trace.record(TraceEvent::LiveResultsApplyAttempt {
            query_id: query_id.to_string(),
            mutation_count: mutations.len(),
        });
        if self.fail_live_results.load(Ordering::Acquire) {
            return Err(Self::injected_failure());
        }
        let mut state = self.state.lock().unwrap();
        let rows = state
            .stage()?
            .live_results
            .entry(query_id.to_string())
            .or_default();
        for mutation in mutations {
            match mutation.data {
                Some(data) => {
                    rows.insert(mutation.row_signature, data.to_vec());
                }
                None => {
                    rows.remove(&mutation.row_signature);
                }
            }
        }
        Ok(())
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed
            .live_results
            .get(query_id)
            .map(|rows| {
                rows.iter()
                    .map(|(signature, data)| (*signature, data.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .clearable()
            .live_results
            .remove(query_id);
        Ok(())
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed
            .live_results
            .get(query_id)
            .map_or(0, BTreeMap::len))
    }
}

struct CharacterizationBackend {
    trace: EventTrace,
    element_index: Arc<TransactionalIndexes>,
    result_index: Arc<TransactionalIndexes>,
    future_queue: Arc<TransactionalIndexes>,
    session_control: Arc<RecordingSessionControl>,
    checkpoint_store: Arc<TransactionalCheckpointStore>,
    output_store: Arc<RecordingOutputStore>,
}

impl CharacterizationBackend {
    fn new(fail_output_writes: bool) -> Self {
        let trace = EventTrace::default();
        let state = Arc::new(Mutex::new(TransactionState::default()));
        let indexes = Arc::new(TransactionalIndexes {
            state: state.clone(),
        });
        Self {
            trace: trace.clone(),
            element_index: indexes.clone(),
            result_index: indexes.clone(),
            future_queue: indexes,
            session_control: Arc::new(RecordingSessionControl {
                state: state.clone(),
                trace: trace.clone(),
                output_gate: Mutex::new(None),
            }),
            checkpoint_store: Arc::new(TransactionalCheckpointStore {
                state: state.clone(),
                trace: trace.clone(),
            }),
            output_store: Arc::new(RecordingOutputStore::new(fail_output_writes, trace, state)),
        }
    }
}

#[async_trait]
impl IndexBackendPlugin for CharacterizationBackend {
    fn supports_atomic_query_output(&self) -> bool {
        true
    }
    async fn create_indexes(&self, _query_id: &str) -> Result<CreatedIndexes, IndexError> {
        Ok(CreatedIndexes {
            set: IndexSet {
                element_index: self.element_index.clone(),
                archive_index: self.element_index.clone(),
                result_index: self.result_index.clone(),
                future_queue: self.future_queue.clone(),
                session_control: self.session_control.clone(),
            },
            checkpoint_store: Some(self.checkpoint_store.clone()),
            outbox_writer: Some(self.output_store.clone()),
            live_results_writer: Some(self.output_store.clone()),
        })
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

struct CharacterizationSource {
    base: SourceBase,
    subscription_tx: mpsc::UnboundedSender<crate::config::SourceSubscriptionSettings>,
}

impl CharacterizationSource {
    fn new(
        id: &str,
        subscription_tx: mpsc::UnboundedSender<crate::config::SourceSubscriptionSettings>,
    ) -> Self {
        Self {
            base: SourceBase::new(SourceBaseParams::new(id)).unwrap(),
            subscription_tx,
        }
    }

    async fn inject(
        &self,
        change: SourceChange,
        sequence: u64,
        source_position: Bytes,
    ) -> anyhow::Result<()> {
        let timestamp =
            chrono::DateTime::from_timestamp_millis(change.get_realtime() as i64).unwrap();
        let mut event = SourceEventWrapper::with_sequence(
            self.id().to_string(),
            SourceEvent::Change(change),
            timestamp,
            sequence,
            Some(ProfilingMetadata::default()),
        );
        event.set_source_position(source_position);
        self.base.dispatch_event(event).await
    }
}

#[async_trait]
impl Source for CharacterizationSource {
    fn supports_replay(&self) -> bool {
        true
    }
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "pipeline-characterization"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Starting, Some("Starting".to_string()))
            .await;
        self.base
            .set_status(ComponentStatus::Running, Some("Running".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Stopping, Some("Stopping".to_string()))
            .await;
        self.base
            .set_status(ComponentStatus::Stopped, Some("Stopped".to_string()))
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status_handle().get_status().await
    }

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.subscription_tx
            .send(settings.clone())
            .map_err(|_| anyhow::anyhow!("test subscription receiver dropped"))?;
        self.base
            .subscribe_with_bootstrap(&settings, self.type_name())
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: crate::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.base.remove_position_handle(query_id).await;
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn crate::BootstrapProvider>) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

struct PipelineHarness {
    core: crate::DrasiLib,
    subscription_rx: mpsc::UnboundedReceiver<crate::config::SourceSubscriptionSettings>,
}

impl PipelineHarness {
    async fn new(backend: Arc<CharacterizationBackend>) -> Self {
        let (subscription_tx, subscription_rx) = mpsc::unbounded_channel();
        let source = CharacterizationSource::new(SOURCE_ID, subscription_tx);
        let core = crate::DrasiLib::builder()
            .with_id("pipeline-characterization")
            .with_index_provider("characterization", backend)
            .with_source(source)
            .build()
            .await
            .unwrap();
        core.start_source(SOURCE_ID).await.unwrap();
        Self {
            core,
            subscription_rx,
        }
    }

    async fn start_query(&self, trace: EventTrace) -> (Arc<dyn Query>, ResultReceiver) {
        self.start_config(query_config(), trace).await
    }

    async fn start_config(
        &self,
        config: QueryConfig,
        trace: EventTrace,
    ) -> (Arc<dyn Query>, ResultReceiver) {
        if self.core.get_query_config(QUERY_ID).await.is_ok() {
            self.core.update_query(QUERY_ID, config).await.unwrap();
        } else {
            self.core.add_query(config).await.unwrap();
        }
        self.core.start_query(QUERY_ID).await.unwrap();
        self.core
            .computation_component(QUERY_ID)
            .unwrap()
            .wait_started()
            .await
            .unwrap();
        let query = self
            .core
            .query_manager()
            .get_query_instance(QUERY_ID)
            .await
            .unwrap();
        let receiver = query.subscribe("recording".into()).await.unwrap().receiver;
        (query, ResultReceiver { receiver, trace })
    }

    async fn next_subscription(&mut self) -> crate::config::SourceSubscriptionSettings {
        tokio::time::timeout(Duration::from_secs(5), self.subscription_rx.recv())
            .await
            .expect("source subscription should arrive")
            .expect("source subscription channel should remain open")
    }

    async fn inject(&self, change: SourceChange, sequence: u64, source_position: Bytes) {
        let source = self.core.source_instance(SOURCE_ID).await.unwrap();
        source
            .as_any()
            .downcast_ref::<CharacterizationSource>()
            .unwrap()
            .inject(change, sequence, source_position)
            .await
            .unwrap();
    }
}

struct ResultReceiver {
    receiver: Box<dyn ChangeReceiver<QueryResult>>,
    trace: EventTrace,
}
impl ResultReceiver {
    fn try_recv(&mut self) -> Result<Arc<QueryResult>, ()> {
        self.receiver
            .recv()
            .now_or_never()
            .ok_or(())?
            .map_err(|_| ())
    }
}

fn query_config() -> QueryConfig {
    QueryConfig {
        id: QUERY_ID.to_string(),
        query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
        query_language: QueryLanguage::Cypher,
        middleware: vec![],
        sources: vec![SourceSubscriptionConfig {
            source_id: SOURCE_ID.to_string(),
            nodes: vec![],
            relations: vec![],
            pipeline: vec![],
        }],
        auto_start: false,
        joins: None,
        enable_bootstrap: false,
        bootstrap_buffer_size: 100,
        priority_queue_capacity: None,
        dispatch_buffer_capacity: None,
        dispatch_mode: None,
        storage_backend: Some(StorageBackendRef::Named("characterization".to_string())),
        recovery_policy: None,
        outbox_capacity: 16,
        bootstrap_timeout_secs: 5,
    }
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

fn variables(entries: &[(&str, VariableValue)]) -> QueryVariables {
    entries
        .iter()
        .map(|(key, value)| ((*key).into(), value.clone()))
        .collect()
}

async fn next_result(receiver: &mut ResultReceiver) -> Arc<QueryResult> {
    let result = tokio::time::timeout(Duration::from_secs(5), receiver.receiver.recv())
        .await
        .expect("query result should arrive")
        .expect("query result channel should remain open");
    receiver.trace.record(TraceEvent::ReactionDispatch {
        sequence: result.sequence,
    });
    result
}

struct CountingBootstrap(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl crate::BootstrapProvider for CountingBootstrap {
    async fn bootstrap(
        &self,
        _: crate::bootstrap::BootstrapRequest,
        _: &crate::bootstrap::BootstrapContext,
        _: crate::channels::BootstrapEventSender,
        _: Option<&crate::SourceSubscriptionSettings>,
    ) -> anyhow::Result<crate::bootstrap::BootstrapResult> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(crate::bootstrap::BootstrapResult::default())
    }
}

#[tokio::test]
async fn strict_restart_keeps_sequence_only_checkpoint_output() {
    let backend = Arc::new(CharacterizationBackend::new(false));
    let harness = PipelineHarness::new(backend.clone()).await;
    let (query, mut receiver) = harness.start_query(backend.trace.clone()).await;
    let source = harness.core.source_instance(SOURCE_ID).await.unwrap();
    let base = &source
        .as_any()
        .downcast_ref::<CharacterizationSource>()
        .unwrap()
        .base;
    base.dispatch_event(SourceEventWrapper::with_sequence(
        SOURCE_ID.into(),
        SourceEvent::Change(person_insert("one", "Alice", 1000)),
        chrono::Utc::now(),
        1,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(next_result(&mut receiver).await.sequence, 1);
    let checkpoint = backend
        .checkpoint_store
        .read_checkpoint(&progress_key("", Some(SOURCE_ID)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.sequence, 1);
    assert!(checkpoint.source_position.is_none());

    query.stop().await.unwrap();
    backend.trace.clear();
    query.start().await.unwrap();
    harness
        .core
        .computation_component(QUERY_ID)
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let snapshot = query.fetch_snapshot().await.unwrap();
    assert_eq!(snapshot.as_of_sequence, 1);
    assert_eq!(snapshot.to_vec(), vec![json!({"name": "Alice"})]);
    assert_eq!(query.fetch_outbox(0).await.unwrap().results[0].sequence, 1);
    assert!(
        !backend.trace.snapshot().iter().any(|event| matches!(
            event, TraceEvent::CheckpointStaged { source_id, sequence: 0, .. }
                if source_id == "\0computation:query-bootstrap:v1"
        )),
        "a replay-only restart must not mark a completed bootstrap incomplete"
    );

    let mut output = query
        .subscribe("after-restart".into())
        .await
        .unwrap()
        .receiver;
    base.dispatch_event(SourceEventWrapper::with_sequence(
        SOURCE_ID.into(),
        SourceEvent::Change(person_insert("two", "Bob", 2000)),
        chrono::Utc::now(),
        2,
        None,
    ))
    .await
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.sequence, 2);
    assert!(matches!(&result.results[..], [ResultDiff::Add { data, .. }]
        if data == &json!({"name": "Bob"})));
    harness.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn strict_reconstruction_bootstraps_without_source_checkpoint() {
    let backend = Arc::new(CharacterizationBackend::new(false));
    let bootstraps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for expected in 1..=2 {
        let mut harness = PipelineHarness::new(backend.clone()).await;
        harness
            .core
            .source_instance(SOURCE_ID)
            .await
            .unwrap()
            .set_bootstrap_provider(Box::new(CountingBootstrap(bootstraps.clone())))
            .await;
        let mut config = query_config();
        config.enable_bootstrap = true;
        let (query, _) = harness.start_config(config, backend.trace.clone()).await;
        let subscription = harness.next_subscription().await;
        assert!(subscription.enable_bootstrap);
        assert!(subscription.resume_from.is_none());
        assert!(subscription.resume_sequence.is_none());
        assert_eq!(bootstraps.load(Ordering::Acquire), expected);
        assert!(backend
            .checkpoint_store
            .read_checkpoint(&progress_key("", Some(SOURCE_ID)))
            .await
            .unwrap()
            .is_none());
        assert_eq!(query.fetch_snapshot().await.unwrap().as_of_sequence, 0);
        harness.core.shutdown().await.unwrap();
    }
}

fn decode_result(bytes: &[u8]) -> QueryResult {
    let mut codec = EnvelopeCodec::new(std::num::NonZeroUsize::new(64 * 1024 * 1024).unwrap());
    codec.register_schema(QueryChangeCodec::schema()).unwrap();
    QueryChangeCodec::to_legacy_result(&codec.decode(bytes).unwrap()).unwrap()
}

fn assert_commit_precedes_dispatch(trace: &[TraceEvent], sequence: u64) {
    let output = trace
        .iter()
        .position(|event| {
            matches!(event,
        TraceEvent::OutboxAppendAttempt { sequence: value, .. } if *value == sequence)
        })
        .unwrap();
    let live = trace
        .iter()
        .position(|event| matches!(event, TraceEvent::LiveResultsApplyAttempt { .. }))
        .unwrap();
    let staged = trace
        .iter()
        .position(|event| {
            matches!(event,
        TraceEvent::ResultSequenceStaged { sequence: value, .. } if *value == sequence)
        })
        .unwrap();
    let commit = trace
        .iter()
        .enumerate()
        .find(|(index, event)| *index > output && matches!(event, TraceEvent::SessionCommit))
        .unwrap()
        .0;
    let dispatch = trace
        .iter()
        .position(|event| {
            matches!(event,
        TraceEvent::ReactionDispatch { sequence: value } if *value == sequence)
        })
        .unwrap();
    assert!(
        output < commit && live < commit && staged < commit && commit < dispatch,
        "{trace:?}"
    );
}

fn assert_native_evaluation_codec() {
    use crate::computation::v1::{ComponentId, QueryOutputMetadata, StreamId, SystemMetadata};
    let contexts = vec![
        QueryPartEvaluationContext::Adding {
            after: variables(&[("kind", VariableValue::String("added".into()))]),
            row_signature: 11,
        },
        QueryPartEvaluationContext::Noop,
        QueryPartEvaluationContext::Updating {
            before: variables(&[("kind", VariableValue::String("before".into()))]),
            after: variables(&[("kind", VariableValue::String("after".into()))]),
            row_signature: 22,
        },
        QueryPartEvaluationContext::Removing {
            before: variables(&[("kind", VariableValue::String("removed".into()))]),
            row_signature: 33,
        },
        QueryPartEvaluationContext::Aggregation {
            before: Some(variables(&[("total", VariableValue::Integer(1.into()))])),
            after: variables(&[("total", VariableValue::Integer(2.into()))]),
            grouping_keys: vec!["group".into()],
            default_before: false,
            default_after: false,
            row_signature: 44,
        },
    ];
    let metadata = HashMap::from([
        ("source_id".into(), json!("source-a")),
        ("processed_by".into(), json!("drasi-core")),
        ("result_count".into(), json!(4)),
    ]);
    let output = QueryChangeCodec::encode_evaluation(
        None,
        &ComponentId::try_new("query-a").unwrap(),
        SystemMetadata::new(StreamId::try_new("query-a/out").unwrap(), 1),
        &contexts,
        QueryOutputMetadata {
            query_id: "query-a".into(),
            source_id: Some("source-a".into()),
            timestamp: chrono::Utc::now(),
            metadata: metadata.clone(),
            profiling: Some(ProfilingMetadata::default()),
        },
    )
    .unwrap()
    .unwrap();
    let result = QueryChangeCodec::to_legacy_result(&output).unwrap();
    assert_eq!(result.query_id, "query-a");
    assert_eq!(result.sequence, 1);
    assert_eq!(result.metadata, metadata);
    assert_eq!(result.profiling, Some(ProfilingMetadata::default()));
    assert_eq!(
        result.results,
        vec![
            ResultDiff::Add {
                data: json!({"kind": "added"}),
                row_signature: 11
            },
            ResultDiff::Update {
                data: json!({"kind": "after"}),
                before: json!({"kind": "before"}),
                after: json!({"kind": "after"}),
                grouping_keys: None,
                row_signature: 22,
            },
            ResultDiff::Delete {
                data: json!({"kind": "removed"}),
                row_signature: 33
            },
            ResultDiff::Aggregation {
                before: Some(json!({"total": 1})),
                after: json!({"total": 2}),
                row_signature: 44,
            },
        ]
    );
}

#[tokio::test]
async fn evaluation_results_map_to_ordered_query_result_and_sequence() {
    assert_native_evaluation_codec();
    let backend = Arc::new(CharacterizationBackend::new(false));
    let harness = PipelineHarness::new(backend.clone()).await;
    let (query, mut receiver) = harness.start_query(backend.trace.clone()).await;
    for (sequence, name) in [(1, "Alice"), (2, "Bob")] {
        backend.trace.clear();
        let gate = Arc::new(CommitGate::default());
        *backend.session_control.output_gate.lock().unwrap() = Some(gate.clone());
        harness
            .inject(
                person_insert(&sequence.to_string(), name, sequence * 1000),
                sequence,
                Bytes::from(sequence.to_string()),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            backend
                .output_store
                .read_from(QUERY_ID, 0)
                .await
                .unwrap()
                .len(),
            sequence as usize - 1
        );
        assert_eq!(
            backend.output_store.row_count(QUERY_ID).await.unwrap(),
            sequence as usize - 1
        );
        assert_eq!(
            backend
                .checkpoint_store
                .read_result_sequence(QUERY_ID)
                .await
                .unwrap()
                .unwrap_or(0),
            sequence - 1
        );
        assert!(
            receiver.try_recv().is_err(),
            "uncommitted results must never reach subscribers"
        );
        gate.release.notify_one();
        let result = next_result(&mut receiver).await;
        assert_eq!(result.sequence, sequence);
        assert_eq!(result.metadata["source_id"], json!(SOURCE_ID));
        assert_eq!(result.metadata["processed_by"], json!("drasi-core"));
        assert_eq!(result.metadata["result_count"], json!(1));
        assert_commit_precedes_dispatch(&backend.trace.snapshot(), sequence);
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(snapshot.as_of_sequence, sequence);
        assert_eq!(snapshot.to_vec().len(), sequence as usize);
        let outbox = query.fetch_outbox(sequence - 1).await.unwrap();
        assert_eq!(outbox.results.len(), 1);
        assert_eq!(outbox.results[0].results, result.results);
        assert_eq!(
            backend
                .checkpoint_store
                .read_result_sequence(QUERY_ID)
                .await
                .unwrap(),
            Some(sequence)
        );
    }
    harness.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_input_has_no_recoverable_output_when_persistence_fails() {
    let backend = Arc::new(CharacterizationBackend::new(true));
    let mut harness = PipelineHarness::new(backend.clone()).await;

    let (query, mut result_rx) = harness.start_query(backend.trace.clone()).await;
    let initial_subscription = harness.next_subscription().await;
    assert_eq!(initial_subscription.resume_sequence, None);
    assert_eq!(initial_subscription.resume_from, None);
    let initial_result_sequence = backend
        .checkpoint_store
        .read_result_sequence(QUERY_ID)
        .await
        .unwrap();
    assert_eq!(initial_result_sequence.unwrap_or(0), 0);
    let mut events = harness.core.subscribe_all_component_events();
    backend.trace.clear();

    let first_position = Bytes::from_static(b"position-1");
    harness
        .inject(
            person_insert("person-1", "Alice", 1_000),
            1,
            first_position.clone(),
        )
        .await;
    crate::test_helpers::wait_for_component_status(
        &mut events,
        QUERY_ID,
        ComponentStatus::Error,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        result_rx.try_recv().is_err(),
        "failed output must not be dispatched"
    );

    assert_eq!(
        backend.trace.snapshot(),
        vec![
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: progress_key("", Some(SOURCE_ID)),
                sequence: 1,
                source_position: Some(first_position.clone()),
            },
            TraceEvent::OutboxAppendAttempt {
                query_id: QUERY_ID.to_string(),
                sequence: 1,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: QUERY_ID.to_string(),
                mutation_count: 1,
            },
            TraceEvent::SessionRollback,
        ],
        "an output failure must roll back input progress and the already staged outbox append"
    );

    assert!(
        backend
            .element_index
            .get_element(&ElementReference::new(SOURCE_ID, "person-1"))
            .await
            .unwrap()
            .is_none(),
        "core element state must roll back with output"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_checkpoint(&progress_key("", Some(SOURCE_ID)))
            .await
            .unwrap(),
        None,
        "source progress must not commit without its output"
    );
    assert!(
        backend
            .output_store
            .read_from(QUERY_ID, 0)
            .await
            .unwrap()
            .is_empty(),
        "the outbox append preceding the live-result failure must be rolled back"
    );
    assert!(
        backend
            .output_store
            .read_snapshot(QUERY_ID)
            .await
            .unwrap()
            .is_empty(),
        "failed live-result persistence leaves no recoverable snapshot"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .unwrap(),
        initial_result_sequence,
        "failed output writes must not advance the durable result sequence"
    );
    assert_eq!(query.output_metrics().unwrap().snapshot().outbox_size, 0);
    query.stop().await.unwrap();

    backend
        .output_store
        .fail_live_results
        .store(false, Ordering::Release);
    let (recovery_query, mut recovery_result_rx) = harness.start_query(backend.trace.clone()).await;
    let retry_subscription = harness.next_subscription().await;
    assert_eq!(retry_subscription.resume_sequence, Some(0));
    assert_eq!(retry_subscription.resume_from, None);
    harness
        .inject(
            person_insert("person-1", "Alice", 1_000),
            1,
            first_position.clone(),
        )
        .await;
    let recovered = next_result(&mut recovery_result_rx).await;
    assert_eq!(recovered.sequence, 1);
    assert_eq!(recovered.results.len(), 1);
    assert!(
        matches!(
            &recovered.results[0],
            ResultDiff::Add { data, .. } if data == &json!({ "name": "Alice" })
        ),
        "the failed input must still be replayable and produce its original Add result"
    );
    assert!(
        backend
            .element_index
            .get_element(&ElementReference::new(SOURCE_ID, "person-1"))
            .await
            .unwrap()
            .is_some(),
        "successful replay commits the core element"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_checkpoint(&progress_key("", Some(SOURCE_ID)))
            .await
            .unwrap(),
        Some(SourceCheckpoint::new(1, Some(first_position.clone())))
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .unwrap(),
        Some(1)
    );
    let persisted = backend.output_store.read_from(QUERY_ID, 0).await.unwrap();
    assert_eq!(persisted.len(), 1);
    let persisted_result = decode_result(&persisted[0].1);
    assert_eq!(persisted_result.sequence, recovered.sequence);
    assert_eq!(persisted_result.results, recovered.results);
    assert_eq!(persisted_result.timestamp, recovered.timestamp);
    recovery_query.stop().await.unwrap();

    let (restarted_query, mut restarted_result_rx) =
        harness.start_query(backend.trace.clone()).await;
    let resumed_subscription = harness.next_subscription().await;
    assert_eq!(resumed_subscription.resume_sequence, Some(1));
    assert_eq!(resumed_subscription.resume_from, Some(first_position));

    let recovered_snapshot = restarted_query.fetch_snapshot().await.unwrap();
    assert_eq!(recovered_snapshot.as_of_sequence, 1);
    assert_eq!(
        recovered_snapshot.to_vec(),
        vec![json!({ "name": "Alice" })],
        "a fresh query must hydrate the output committed with source position 1"
    );

    backend.trace.clear();
    harness
        .inject(
            person_insert("person-1", "Alice", 1_000),
            1,
            Bytes::from_static(b"position-2"),
        )
        .await;
    harness
        .inject(
            person_insert("person-2", "Bob", 2_000),
            2,
            Bytes::from_static(b"position-3"),
        )
        .await;

    let after_restart = next_result(&mut restarted_result_rx).await;
    assert_eq!(after_restart.sequence, 2);
    assert_eq!(after_restart.results.len(), 1);
    assert!(
        matches!(
            &after_restart.results[0],
            ResultDiff::Add { data, .. } if data == &json!({ "name": "Bob" })
        ),
        "checkpoint-seeded dedup must suppress replayed source sequence 1",
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while backend
            .trace
            .snapshot()
            .iter()
            .filter(|event| matches!(event, TraceEvent::SessionCommit))
            .count()
            < 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("query confirms the accepted output");
    let mut trace = backend.trace.snapshot();
    let commit = trace
        .iter()
        .position(|event| matches!(event, TraceEvent::SessionCommit))
        .unwrap();
    let dispatch = trace
        .iter()
        .position(|event| matches!(event, TraceEvent::ReactionDispatch { sequence: 2 }))
        .unwrap();
    assert!(
        dispatch > commit,
        "external dispatch follows the input/state/output commit"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, TraceEvent::ReactionDispatch { .. }))
            .count(),
        1
    );
    trace.retain(|event| !matches!(event, TraceEvent::ReactionDispatch { .. }));
    assert_eq!(
        trace,
        vec![
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: progress_key("", Some(SOURCE_ID)),
                sequence: 2,
                source_position: Some(Bytes::from_static(b"position-3")),
            },
            TraceEvent::OutboxAppendAttempt {
                query_id: QUERY_ID.to_string(),
                sequence: 2,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: QUERY_ID.to_string(),
                mutation_count: 1,
            },
            TraceEvent::ResultSequenceStaged {
                query_id: QUERY_ID.to_string(),
                sequence: 2,
            },
            TraceEvent::CheckpointStaged {
                source_id: "\0computation:query-delivery-sequence:v1".into(),
                sequence: 2,
                source_position: None,
            },
            TraceEvent::SessionCommit,
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: "\0computation:query-delivered:v1".into(),
                sequence: 2,
                source_position: None,
            },
            TraceEvent::SessionCommit,
        ],
        "committed input is deduplicated before a session, and new output commits before dispatch"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_checkpoint(&progress_key("", Some(SOURCE_ID)))
            .await
            .unwrap(),
        Some(SourceCheckpoint::new(
            2,
            Some(Bytes::from_static(b"position-3"))
        ))
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        backend
            .output_store
            .read_from(QUERY_ID, 0)
            .await
            .unwrap()
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(backend.output_store.row_count(QUERY_ID).await.unwrap(), 2);

    restarted_query.stop().await.unwrap();
    harness.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn due_future_output_is_dispatched_after_core_commit() {
    let backend = Arc::new(CharacterizationBackend::new(false));
    let harness = PipelineHarness::new(backend.clone()).await;
    let mut config = query_config();
    config.query = "
        MATCH (o:Order)
        WHERE o.status = 'ready'
          AND drasi.trueFor(sum(o.value) > 1, duration({ seconds: 5 }))
        RETURN sum(o.value) AS value
        "
    .into();
    let (query, mut result_rx) = harness.start_config(config, backend.trace.clone()).await;
    let gate = Arc::new(CommitGate::default());
    *backend.session_control.output_gate.lock().unwrap() = Some(gate.clone());
    backend.trace.clear();
    harness
        .inject(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(SOURCE_ID, "order-1"),
                        labels: vec!["Order".into()].into(),
                        effective_from: 1_577_836_800_000,
                    },
                    properties: ElementPropertyMap::from(json!({
                        "status": "ready",
                        "value": 2,
                    })),
                },
            },
            1,
            Bytes::from_static(b"order-position"),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        backend
            .future_queue
            .state
            .lock()
            .unwrap()
            .committed
            .futures
            .len(),
        1,
        "the future pop must not commit ahead of its output"
    );
    assert!(backend
        .output_store
        .read_from(QUERY_ID, 0)
        .await
        .unwrap()
        .is_empty());
    assert!(backend
        .output_store
        .read_snapshot(QUERY_ID)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .unwrap()
            .unwrap_or(0),
        0
    );
    assert!(result_rx.try_recv().is_err());
    gate.release.notify_one();
    let dispatched = next_result(&mut result_rx).await;
    assert_eq!(dispatched.sequence, 1);
    assert_eq!(dispatched.metadata["source_id"], json!(SOURCE_ID));
    assert_commit_precedes_dispatch(&backend.trace.snapshot(), 1);
    assert!(backend
        .future_queue
        .peek_due_time()
        .await
        .unwrap()
        .is_none());
    let persisted = backend.output_store.read_from(QUERY_ID, 0).await.unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].0, 1);
    assert_eq!(decode_result(&persisted[0].1).results, dispatched.results);
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .unwrap(),
        Some(1)
    );
    let snapshot = query.fetch_snapshot().await.unwrap();
    assert_eq!(snapshot.as_of_sequence, 1);
    assert_eq!(snapshot.to_vec(), vec![json!({"value": 2.0})]);
    assert_eq!(backend.output_store.row_count(QUERY_ID).await.unwrap(), 1);
    harness.core.shutdown().await.unwrap();
}
