// Copyright 2024 The Drasi Authors.
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

use std::sync::Arc;

use crate::{
    evaluation::{context::QueryVariables, variable_value::VariableValue},
    models::ElementReference,
};
use async_trait::async_trait;
use ordered_float::OrderedFloat;

use crate::evaluation::functions::aggregation::ValueAccumulator;

use super::IndexError;

pub const RESULT_INDEX_STATE_VERSION: u64 = 4;

#[async_trait]
pub trait ResultIndex: AccumulatorIndex + ResultSequenceCounter {
    /// Ensures the accumulator store uses the current result-index state format.
    ///
    /// A missing version marker is initialized only when the store is empty.
    /// Markerless non-empty stores predate the current format and must be
    /// cleared and replayed rather than interpreted with incomplete state.
    async fn ensure_state_version(&self) -> Result<(), IndexError> {
        let key = ResultKey::InputHash(0);
        let owner = ResultOwner::QueryState;

        match self.get(&key, &owner).await? {
            Some(ValueAccumulator::Signature(RESULT_INDEX_STATE_VERSION)) => Ok(()),
            Some(ValueAccumulator::Signature(version)) => Err(IndexError::other(
                ResultIndexStateVersionError::UnsupportedVersion {
                    expected: RESULT_INDEX_STATE_VERSION,
                    actual: version,
                },
            )),
            Some(_) => Err(IndexError::other(
                ResultIndexStateVersionError::InvalidMarker,
            )),
            None if self.is_empty().await? => {
                self.set(
                    key,
                    owner,
                    Some(ValueAccumulator::Signature(RESULT_INDEX_STATE_VERSION)),
                )
                .await
            }
            None => Err(IndexError::other(
                ResultIndexStateVersionError::MissingMarker,
            )),
        }
    }
}

#[async_trait]
pub trait AccumulatorIndex: LazySortedSetStore {
    /// Returns true when the accumulator and sorted-set stores contain no state.
    async fn is_empty(&self) -> Result<bool, IndexError>;

    async fn clear(&self) -> Result<(), IndexError>;
    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError>;
    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError>;
}

#[async_trait]
pub trait LazySortedSetStore: Send + Sync {
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError>;
    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError>;
    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSequence {
    pub sequence: u64,
    pub source_change_id: Arc<str>,
}

impl Default for ResultSequence {
    fn default() -> Self {
        ResultSequence {
            sequence: 0,
            source_change_id: Arc::from(""),
        }
    }
}

/// Tracks the monotonic output sequence number for a query's result stream.
///
/// Each time a query emits a result batch, the sequence is incremented and
/// persisted so that downstream consumers (reactions) can detect ordering
/// and gaps. This is separate from the source checkpoint tracking in
/// [`CheckpointStore`](super::CheckpointStore).
#[async_trait]
pub trait ResultSequenceCounter: Send + Sync {
    async fn apply_sequence(&self, sequence: u64, source_change_id: &str)
        -> Result<(), IndexError>;
    async fn get_sequence(&self) -> Result<ResultSequence, IndexError>;
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub enum ResultOwner {
    Function(usize),
    PartCurrent(usize),
    PartDefault(usize),
    PartGroupCardinality(usize),
    QueryState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResultKey {
    GroupBy(Arc<Vec<VariableValue>>),
    InputHash(u64),
    Element(ElementReference),
}

#[derive(Debug, thiserror::Error)]
enum ResultIndexStateVersionError {
    #[error(
        "result index has no state-version marker but contains data; clear and replay the query index"
    )]
    MissingMarker,
    #[error("result index state-version marker has an incompatible value")]
    InvalidMarker,
    #[error("unsupported result index state version {actual}; expected {expected}")]
    UnsupportedVersion { expected: u64, actual: u64 },
}

impl ResultKey {
    pub fn groupby_from_variables(keys: &[String], variables: &QueryVariables) -> ResultKey {
        let mut grouping_keys = Vec::new();
        for key in keys.iter() {
            grouping_keys.push(
                variables
                    .get(key.as_str())
                    .unwrap_or(&VariableValue::Null)
                    .clone(),
            );
        }
        ResultKey::GroupBy(Arc::new(grouping_keys))
    }
}

impl std::hash::Hash for ResultKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            ResultKey::GroupBy(grouping_keys) => {
                for key in grouping_keys.iter() {
                    key.hash_for_groupby(state);
                }
            }
            ResultKey::InputHash(hash) => {
                hash.hash(state);
            }
            ResultKey::Element(element_reference) => {
                element_reference.hash(state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

    #[tokio::test]
    async fn state_version_initializes_only_empty_indexes() {
        let empty = InMemoryResultIndex::new();
        empty.ensure_state_version().await.unwrap();
        empty.ensure_state_version().await.unwrap();
        assert!(!empty.is_empty().await.unwrap());

        let legacy = InMemoryResultIndex::new();
        legacy
            .set(
                ResultKey::InputHash(42),
                ResultOwner::Function(1),
                Some(ValueAccumulator::Count { value: 1 }),
            )
            .await
            .unwrap();
        let error = legacy.ensure_state_version().await.unwrap_err();
        assert!(
            matches!(&error, IndexError::Other(source) if source.to_string().contains("clear and replay"))
        );
    }

    #[tokio::test]
    async fn incompatible_state_version_is_rejected() {
        let index = InMemoryResultIndex::new();
        index
            .set(
                ResultKey::InputHash(0),
                ResultOwner::QueryState,
                Some(ValueAccumulator::Signature(RESULT_INDEX_STATE_VERSION - 1)),
            )
            .await
            .unwrap();

        let error = index.ensure_state_version().await.unwrap_err();
        assert!(
            matches!(&error, IndexError::Other(source) if source.to_string().contains("unsupported result index state version"))
        );
    }
}
