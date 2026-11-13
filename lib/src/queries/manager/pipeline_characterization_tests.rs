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
        ElementStream, FutureElementRef, FutureQueue, IndexBackendPlugin, IndexError, IndexJoin,
        IndexSet, LazySortedSetStore, LiveResultsWriter, OutboxWriter, PushType, ResultIndex,
        ResultKey, ResultOwner, ResultSequence, ResultSequenceCounter, RowMutation, SessionControl,
        SourceCheckpoint,
    },
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp,
        SourceChange, TimestampRange,
    },
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
    computation::v1::{EnvelopeCodec, QueryChangeCodec, StreamId},
    config::{QueryInputConfig, QueryLanguage},
    indexes::config::StorageBackendRef,
    profiling::ProfilingMetadata,
    sources::{Source, SourceBase, SourceBaseParams},
};

const SOURCE_ID: &str = "pipeline-source";

fn source_component_id() -> crate::ComponentId {
    crate::ComponentId::try_new("00000000-0000-0000-0000-00000000036a").expect("fixture UUID")
}

fn query_component_id() -> crate::ComponentId {
    crate::ComponentId::try_new("00000000-0000-0000-0000-00000000036c").expect("fixture UUID")
}

fn source_checkpoint_key() -> drasi_core_types::CheckpointKey {
    drasi_core_types::CheckpointKey::Source(source_component_id())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TraceEvent {
    SessionBegin,
    CheckpointStaged {
        source_id: drasi_core_types::CheckpointKey,
        sequence: u64,
        source_position: Option<Bytes>,
    },
    SessionCommit,
    SessionRollback,
    OutboxAppendAttempt {
        query_id: drasi_core_types::OutputPartition,
        sequence: u64,
    },
    LiveResultsApplyAttempt {
        query_id: crate::ComponentId,
        mutation_count: usize,
    },
    ResultSequenceWritten {
        query_id: drasi_core_types::OutputPartition,
        sequence: u64,
    },
    ResultSequenceStaged {
        query_id: drasi_core_types::OutputPartition,
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

type SortedSets = HashMap<u64, BTreeMap<OrderedFloat<f64>, isize>>;

#[derive(Clone, Default)]
struct TransactionData {
    elements: HashMap<ElementReference, (Arc<Element>, Vec<usize>)>,
    accumulators: HashMap<u64, ValueAccumulator>,
    sorted_sets: SortedSets,
    index_sequence: ResultSequence,
    futures: Vec<(usize, FutureElementRef)>,
    checkpoints: HashMap<drasi_core_types::CheckpointKey, SourceCheckpoint>,
    result_sequences: HashMap<drasi_core_types::OutputPartition, u64>,
    outbox: HashMap<drasi_core_types::OutputPartition, BTreeMap<u64, Vec<u8>>>,
    live_results: HashMap<crate::ComponentId, BTreeMap<u64, Vec<u8>>>,
}

#[derive(Default)]
struct TransactionState {
    pending: Option<TransactionData>,
    committed: TransactionData,
    config_hash: Option<u64>,
    output_generations: HashMap<drasi_core_types::OutputPartition, u64>,
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

    async fn set_joins(&self, joins: &[IndexJoin]) -> Result<(), IndexError> {
        assert!(
            joins.is_empty(),
            "the fixture does not implement synthetic joins"
        );
        Ok(())
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
        source_id: &drasi_core_types::CheckpointKey,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let checkpoint = SourceCheckpoint::new(sequence, source_position.cloned());
        let mut state = self.state.lock().unwrap();
        state
            .stage()?
            .checkpoints
            .insert(source_id.clone(), checkpoint.clone());
        self.trace.record(TraceEvent::CheckpointStaged {
            source_id: source_id.clone(),
            sequence,
            source_position: checkpoint.source_position,
        });
        Ok(())
    }

    async fn read_checkpoint(
        &self,
        source_id: &drasi_core_types::CheckpointKey,
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

    async fn read_all_checkpoints(
        &self,
    ) -> Result<HashMap<drasi_core_types::CheckpointKey, SourceCheckpoint>, IndexError> {
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

    async fn stage_result_sequence(
        &self,
        query_id: &drasi_core_types::OutputPartition,
        sequence: u64,
    ) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .stage()?
            .result_sequences
            .insert(query_id.clone(), sequence);
        self.trace.record(TraceEvent::ResultSequenceStaged {
            query_id: query_id.clone(),
            sequence,
        });
        Ok(())
    }

    async fn write_result_sequence(
        &self,
        query_id: &drasi_core_types::OutputPartition,
        sequence: u64,
    ) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        state
            .committed
            .result_sequences
            .insert(query_id.clone(), sequence);
        if let Some(pending) = &mut state.pending {
            pending.result_sequences.insert(query_id.clone(), sequence);
        }
        self.trace.record(TraceEvent::ResultSequenceWritten {
            query_id: query_id.clone(),
            sequence,
        });
        Ok(())
    }

    async fn read_result_sequence(
        &self,
        query_id: &drasi_core_types::OutputPartition,
    ) -> Result<Option<u64>, IndexError> {
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
        query_id: &drasi_core_types::OutputPartition,
        generation: u64,
    ) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .output_generations
            .insert(query_id.clone(), generation);
        Ok(())
    }

    async fn read_output_generation(
        &self,
        query_id: &drasi_core_types::OutputPartition,
    ) -> Result<Option<u64>, IndexError> {
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
    async fn append(
        &self,
        query_id: &drasi_core_types::OutputPartition,
        sequence: u64,
        data: &[u8],
    ) -> Result<(), IndexError> {
        self.trace.record(TraceEvent::OutboxAppendAttempt {
            query_id: query_id.clone(),
            sequence,
        });
        self.state
            .lock()
            .unwrap()
            .stage()?
            .outbox
            .entry(query_id.clone())
            .or_default()
            .insert(sequence, data.to_vec());
        Ok(())
    }

    async fn read_from(
        &self,
        query_id: &drasi_core_types::OutputPartition,
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

    async fn read_latest_sequence(
        &self,
        query_id: &drasi_core_types::OutputPartition,
    ) -> Result<Option<u64>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed
            .outbox
            .get(query_id)
            .and_then(|entries| entries.last_key_value().map(|(sequence, _)| *sequence)))
    }

    async fn clear(&self, query_id: &drasi_core_types::OutputPartition) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .clearable()
            .outbox
            .remove(query_id);
        Ok(())
    }

    async fn trim_before(
        &self,
        query_id: &drasi_core_types::OutputPartition,
        retain_from: u64,
    ) -> Result<usize, IndexError> {
        let mut state = self.state.lock().unwrap();
        let Some(entries) = state.stage()?.outbox.get_mut(query_id) else {
            return Ok(0);
        };
        let previous_len = entries.len();
        entries.retain(|sequence, _| *sequence >= retain_from);
        Ok(previous_len - entries.len())
    }

    async fn trim_to_capacity(
        &self,
        query_id: &drasi_core_types::OutputPartition,
        capacity: usize,
    ) -> Result<usize, IndexError> {
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
        query_id: &drasi_core_types::ComponentId,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        self.trace.record(TraceEvent::LiveResultsApplyAttempt {
            query_id: query_id.clone(),
            mutation_count: mutations.len(),
        });
        if self.fail_live_results.load(Ordering::Acquire) {
            return Err(Self::injected_failure());
        }
        let mut state = self.state.lock().unwrap();
        let rows = state
            .stage()?
            .live_results
            .entry(query_id.clone())
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

    async fn read_snapshot(
        &self,
        query_id: &drasi_core_types::ComponentId,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
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

    async fn clear(&self, query_id: &drasi_core_types::ComponentId) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .clearable()
            .live_results
            .remove(query_id);
        Ok(())
    }

    async fn row_count(
        &self,
        query_id: &drasi_core_types::ComponentId,
    ) -> Result<usize, IndexError> {
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
    owner: std::sync::OnceLock<(crate::StorageScope, crate::ComponentId)>,
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
            owner: std::sync::OnceLock::new(),
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
    async fn create_indexes(
        &self,
        query_id: &drasi_core_types::ComponentId,
    ) -> Result<CreatedIndexes, IndexError> {
        self.create_scoped_indexes(&crate::StorageScope::default(), query_id)
            .await
    }

    async fn create_scoped_indexes(
        &self,
        scope: &crate::StorageScope,
        query_id: &crate::ComponentId,
    ) -> Result<CreatedIndexes, IndexError> {
        let owner = (scope.clone(), query_id.clone());
        if self.owner.get_or_init(|| owner.clone()) != &owner {
            return Err(IndexError::other(std::io::Error::other(
                "characterization fixture has one scoped query owner",
            )));
        }
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
        id: drasi_core_types::ComponentId,
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
            self.id().clone(),
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
    fn id(&self) -> &drasi_core_types::ComponentId {
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

    async fn bind_runtime_context(&self, context: crate::context::SourceRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }

    async fn remove_position_handle(&self, query_id: &drasi_core_types::ComponentId) {
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
        let core = crate::DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("pipeline-characterization")
            .with_allow_external_component_ids(true)
            .with_index_provider("characterization", backend)
            .with_source_factory_async(
                drasi_core_types::ComponentName::try_new("pipeline-source")
                    .expect("fixture component name"),
                Some(source_component_id()),
                true,
                move |id| async move {
                    Ok((
                        CharacterizationSource::new(id, subscription_tx),
                        HashMap::new(),
                    ))
                },
            )
            .build()
            .await
            .unwrap();
        core.start_source_by_name(
            &drasi_core_types::ComponentName::try_new("pipeline-source")
                .expect("fixture component name"),
        )
        .await
        .unwrap();
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
        match self
            .core
            .get_query_config_by_name(
                &drasi_core_types::ComponentName::try_new("pipeline-query")
                    .expect("fixture component name"),
            )
            .await
        {
            Ok(existing) => {
                assert_eq!(
                    serde_json::to_value(existing).unwrap(),
                    serde_json::to_value(config.with_assigned_id(query_component_id())).unwrap(),
                    "this fixture restarts the same definition; configuration changes need an update plan"
                );
            }
            Err(error)
                if matches!(
                    error.classification(),
                    crate::DrasiError::ComponentNotFound { .. }
                ) =>
            {
                self.core.add_query(config).await.unwrap();
            }
            Err(error) => panic!("cannot read the accepted query definition: {error:#}"),
        }
        self.core
            .start_query_by_name(
                &drasi_core_types::ComponentName::try_new("pipeline-query")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        self.core
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("pipeline-query")
                    .expect("fixture component name"),
            )
            .unwrap()
            .wait_started()
            .await
            .unwrap();
        let query = self
            .core
            .query_manager()
            .get_query_instance_by_name(
                &drasi_core_types::ComponentName::try_new("pipeline-query")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        let receiver = query
            .subscribe(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000362")
                    .expect("fixture UUID"),
            )
            .await
            .unwrap()
            .receiver;
        (query, ResultReceiver { receiver, trace })
    }

    async fn next_subscription(&mut self) -> crate::config::SourceSubscriptionSettings {
        tokio::time::timeout(Duration::from_secs(5), self.subscription_rx.recv())
            .await
            .expect("source subscription should arrive")
            .expect("source subscription channel should remain open")
    }

    async fn inject(&self, change: SourceChange, sequence: u64, source_position: Bytes) {
        let source = self
            .core
            .source_instance(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036a")
                    .expect("fixture UUID"),
            )
            .await
            .unwrap();
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
        id: Some(query_component_id()),
        name: drasi_core_types::ComponentName::try_new("pipeline-query")
            .expect("fixture component name"),
        query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
        query_language: QueryLanguage::Cypher,
        middleware: vec![],
        inputs: vec![QueryInputConfig {
            component: crate::config::ComponentReference::by_id(source_component_id()),
            nodes: vec![],
            relations: vec![],
            pipeline: vec![],
            port: None,
        }],
        auto_start: false,
        joins: None,
        enable_bootstrap: false,
        priority_queue_capacity: None,
        dispatch_buffer_capacity: None,
        dispatch_mode: None,
        storage_backend: Some(StorageBackendRef::Named("characterization".to_string())),
        recovery_policy: None,
        outbox_capacity: 16,
        bootstrap_timeout_secs: 5,
        result: Default::default(),
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
    let source = harness
        .core
        .source_instance(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036a")
                .expect("fixture UUID"),
        )
        .await
        .unwrap();
    let base = &source
        .as_any()
        .downcast_ref::<CharacterizationSource>()
        .unwrap()
        .base;
    base.dispatch_event(SourceEventWrapper::with_sequence(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036a")
            .expect("fixture UUID"),
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
        .read_checkpoint(&source_checkpoint_key())
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
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("pipeline-query")
                .expect("fixture component name"),
        )
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
                if source_id.metadata_name() == Some("\0computation:query-bootstrap:v1")
        )),
        "a replay-only restart must not mark a completed bootstrap incomplete"
    );

    let mut output = query
        .subscribe(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036b")
                .expect("fixture UUID"),
        )
        .await
        .unwrap()
        .receiver;
    base.dispatch_event(SourceEventWrapper::with_sequence(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036a")
            .expect("fixture UUID"),
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
async fn cursorless_reconstruction_bootstraps_with_explicit_reset_authorization() {
    let backend = Arc::new(CharacterizationBackend::new(false));
    let bootstraps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut previous_generation = None;
    for expected in 1..=2 {
        let mut harness = PipelineHarness::new(backend.clone()).await;
        harness
            .core
            .source_instance(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036a")
                    .expect("fixture UUID"),
            )
            .await
            .unwrap()
            .set_bootstrap_provider(Box::new(CountingBootstrap(bootstraps.clone())))
            .await;
        let mut config = query_config();
        config.enable_bootstrap = true;
        config.recovery_policy = Some(crate::RecoveryPolicy::AutoReset);
        let (query, _) = harness.start_config(config, backend.trace.clone()).await;
        let subscription = harness.next_subscription().await;
        assert!(subscription.enable_bootstrap);
        assert!(subscription.resume_from.is_none());
        assert!(subscription.resume_sequence.is_none());
        assert_eq!(bootstraps.load(Ordering::Acquire), expected);
        assert!(backend
            .checkpoint_store
            .read_checkpoint(&source_checkpoint_key())
            .await
            .unwrap()
            .is_none());
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(snapshot.as_of_sequence, 0);
        if let Some(previous) = previous_generation {
            assert!(snapshot.output_generation > previous);
        }
        previous_generation = Some(snapshot.output_generation);
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
    use crate::computation::v1::{QueryOutputMetadata, StreamId, SystemMetadata};
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
        &crate::test_helpers::component_id(120),
        SystemMetadata::new(StreamId::try_new("query-a/out").unwrap(), 1),
        &crate::computation::v1::ResultNodeProjector::try_new("test/pipeline/query-a", None)
            .unwrap(),
        &contexts,
        QueryOutputMetadata {
            query_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-000000000078",
            )
            .expect("fixture UUID"),
            input_origin: Some(drasi_computation_contracts::QueryInputOrigin::Source(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000000b5")
                    .expect("fixture UUID"),
            )),
            timestamp: chrono::Utc::now(),
            metadata: metadata.clone(),
            profiling: Some(ProfilingMetadata::default()),
        },
    )
    .unwrap()
    .unwrap();
    let result = QueryChangeCodec::to_legacy_result(&output).unwrap();
    assert_eq!(
        result.query_id,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000078")
            .expect("fixture UUID")
    );
    assert_eq!(result.sequence, 1);
    let mut expected_metadata = metadata;
    expected_metadata.insert(
        "input_origin".into(),
        json!(drasi_computation_contracts::QueryInputOrigin::Source(
            crate::test_helpers::component_id(181)
        )),
    );
    assert_eq!(result.metadata, expected_metadata);
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
                .read_from(
                    &drasi_core_types::OutputPartition::Component(
                        drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-00000000036c"
                        )
                        .expect("fixture UUID")
                    ),
                    0
                )
                .await
                .unwrap()
                .len(),
            sequence as usize - 1
        );
        assert_eq!(
            backend
                .output_store
                .row_count(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                        .expect("fixture UUID")
                )
                .await
                .unwrap(),
            sequence as usize - 1
        );
        assert_eq!(
            backend
                .checkpoint_store
                .read_result_sequence(&drasi_core_types::OutputPartition::Component(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                        .expect("fixture UUID")
                ))
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
        assert_eq!(
            result.metadata["input_origin"],
            json!(drasi_computation_contracts::QueryInputOrigin::Source(
                source_component_id()
            ))
        );
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
                .read_result_sequence(&drasi_core_types::OutputPartition::Component(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                        .expect("fixture UUID")
                ))
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
        .read_result_sequence(&drasi_core_types::OutputPartition::Component(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                .expect("fixture UUID"),
        ))
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
    crate::test_helpers::wait_for_component_status_by_id(
        &mut events,
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
            .expect("fixture UUID"),
        ComponentStatus::Error,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        result_rx.try_recv().is_err(),
        "failed output must not be dispatched"
    );
    let source_component = crate::computation::v1::ComputationPipelineBuilder::source_component_id(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036a")
            .expect("fixture UUID"),
    )
    .unwrap();
    let transport_key = drasi_core_types::CheckpointKey::Stream(
        StreamId::try_new(format!("{source_component}/out")).unwrap(),
    )
    .associated("\0computation:input-transport:v1:");

    assert_eq!(
        backend.trace.snapshot(),
        vec![
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: source_checkpoint_key(),
                sequence: 1,
                source_position: Some(first_position.clone()),
            },
            TraceEvent::CheckpointStaged {
                source_id: transport_key.clone(),
                sequence: 1,
                source_position: None,
            },
            TraceEvent::OutboxAppendAttempt {
                query_id: drasi_core_types::OutputPartition::Component(query_component_id()),
                sequence: 1,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: query_component_id(),
                mutation_count: 1,
            },
            TraceEvent::SessionRollback,
        ],
        "an output failure must roll back source/transport progress and the already staged outbox append"
    );
    assert!(
        backend
            .checkpoint_store
            .read_checkpoint(&transport_key)
            .await
            .unwrap()
            .is_none(),
        "transport progress must roll back with the failed output"
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
            .read_checkpoint(&source_checkpoint_key())
            .await
            .unwrap(),
        None,
        "source progress must not commit without its output"
    );
    assert!(
        backend
            .output_store
            .read_from(
                &drasi_core_types::OutputPartition::Component(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                        .expect("fixture UUID")
                ),
                0
            )
            .await
            .unwrap()
            .is_empty(),
        "the outbox append preceding the live-result failure must be rolled back"
    );
    assert!(
        backend
            .output_store
            .read_snapshot(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            )
            .await
            .unwrap()
            .is_empty(),
        "failed live-result persistence leaves no recoverable snapshot"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(&drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            ))
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
            .read_checkpoint(&source_checkpoint_key())
            .await
            .unwrap(),
        Some(SourceCheckpoint::new(1, Some(first_position.clone())))
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(&drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            ))
            .await
            .unwrap(),
        Some(1)
    );
    let persisted = backend
        .output_store
        .read_from(
            &drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID"),
            ),
            0,
        )
        .await
        .unwrap();
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
            < 3
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
                source_id: transport_key.clone(),
                sequence: 3,
                source_position: None,
            },
            TraceEvent::SessionCommit,
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: source_checkpoint_key(),
                sequence: 2,
                source_position: Some(Bytes::from_static(b"position-3")),
            },
            TraceEvent::CheckpointStaged {
                source_id: transport_key,
                sequence: 4,
                source_position: None,
            },
            TraceEvent::OutboxAppendAttempt {
                query_id: drasi_core_types::OutputPartition::Component(query_component_id()),
                sequence: 2,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: query_component_id(),
                mutation_count: 1,
            },
            TraceEvent::ResultSequenceStaged {
                query_id: drasi_core_types::OutputPartition::Component(query_component_id()),
                sequence: 2,
            },
            TraceEvent::CheckpointStaged {
                source_id: drasi_core_types::CheckpointKey::Metadata(
                    "\0computation:query-delivery-sequence:v1".into(),
                ),
                sequence: 2,
                source_position: None,
            },
            TraceEvent::SessionCommit,
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: drasi_core_types::CheckpointKey::Metadata(
                    "\0computation:query-delivered:v1".into(),
                ),
                sequence: 2,
                source_position: None,
            },
            TraceEvent::SessionCommit,
        ],
        "replayed input advances only transport progress; new logical input and output commit before dispatch"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_checkpoint(&source_checkpoint_key())
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
            .read_result_sequence(&drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            ))
            .await
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        backend
            .output_store
            .read_from(
                &drasi_core_types::OutputPartition::Component(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                        .expect("fixture UUID")
                ),
                0
            )
            .await
            .unwrap()
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        backend
            .output_store
            .row_count(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            )
            .await
            .unwrap(),
        2
    );

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
        .read_from(
            &drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            ),
            0
        )
        .await
        .unwrap()
        .is_empty());
    assert!(backend
        .output_store
        .read_snapshot(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                .expect("fixture UUID")
        )
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(&drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            ))
            .await
            .unwrap()
            .unwrap_or(0),
        0
    );
    assert!(result_rx.try_recv().is_err());
    gate.release.notify_one();
    let dispatched = next_result(&mut result_rx).await;
    assert_eq!(dispatched.sequence, 1);
    assert_eq!(
        dispatched.metadata["input_origin"],
        json!(drasi_computation_contracts::QueryInputOrigin::Scheduled(
            ElementReference::new(SOURCE_ID, "order-1")
        ))
    );
    assert_commit_precedes_dispatch(&backend.trace.snapshot(), 1);
    assert!(backend
        .future_queue
        .peek_due_time()
        .await
        .unwrap()
        .is_none());
    let persisted = backend
        .output_store
        .read_from(
            &drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID"),
            ),
            0,
        )
        .await
        .unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].0, 1);
    assert_eq!(decode_result(&persisted[0].1).results, dispatched.results);
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(&drasi_core_types::OutputPartition::Component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            ))
            .await
            .unwrap(),
        Some(1)
    );
    let snapshot = query.fetch_snapshot().await.unwrap();
    assert_eq!(snapshot.as_of_sequence, 1);
    assert_eq!(snapshot.to_vec(), vec![json!({"value": 2.0})]);
    assert_eq!(
        backend
            .output_store
            .row_count(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000036c")
                    .expect("fixture UUID")
            )
            .await
            .unwrap(),
        1
    );
    harness.core.shutdown().await.unwrap();
}
