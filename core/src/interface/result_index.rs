use std::sync::Arc;

use crate::evaluation::{context::QueryVariables, variable_value::VariableValue};
use async_trait::async_trait;
use ordered_float::OrderedFloat;

use crate::evaluation::functions::aggregation::ValueAccumulator;

use super::IndexError;

pub trait ResultIndex: AccumulatorIndex + ResultSequenceCounter {}

#[async_trait]
pub trait AccumulatorIndex: LazySortedSetStore {
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResultKey {
    GroupBy(Arc<Vec<VariableValue>>),
    InputHash(u64),
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
        }
    }
}
