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
    data_value::DataValue,
    evaluation::{context::QueryVariables, variable_value::VariableValue},
    models::{ElementPropertyMap, ElementReference, ElementValue},
};
use async_trait::async_trait;
use ordered_float::OrderedFloat;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::IndexError;

/// Stored accumulator values shared by evaluators and index providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValueAccumulator {
    Sum {
        value: f64,
    },
    Avg {
        sum: f64,
        count: i64,
    },
    Count {
        value: i64,
    },
    TimeMarker {
        timestamp: u64,
    },
    Signature(u64),
    LinearGradient {
        count: i64,
        mean_x: f64,
        mean_y: f64,
        m2: f64,
        cov: f64,
    },
    Value(ElementValue),
    Map(ElementPropertyMap),
}

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

#[derive(Debug, Clone, PartialEq, Hash, Serialize, Deserialize)]
pub enum ResultOwner {
    Function(usize),
    PartCurrent(usize),
    PartDefault(usize),
    PartTransactionTime(usize),
    PartRealtime(usize),
    /// Source-time deletion frontier; append variants to preserve existing key hashes.
    ElementTombstone,
}

#[derive(Debug, Clone)]
pub enum ResultKey {
    GroupBy(Arc<Vec<VariableValue>>),
    InputHash(u64),
    Element(ElementReference),
}

#[derive(Serialize, Deserialize)]
enum ResultKeyImage {
    GroupBy(Vec<DataValue>),
    InputHash(u64),
    Element(ElementReference),
}

impl Serialize for ResultKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let image = match self {
            Self::GroupBy(values) => ResultKeyImage::GroupBy(
                values
                    .iter()
                    .map(DataValue::encode)
                    .collect::<Result<_, _>>()
                    .map_err(serde::ser::Error::custom)?,
            ),
            Self::InputHash(hash) => ResultKeyImage::InputHash(*hash),
            Self::Element(reference) => ResultKeyImage::Element(reference.clone()),
        };
        image.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResultKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match ResultKeyImage::deserialize(deserializer)? {
            ResultKeyImage::GroupBy(values) => Self::GroupBy(Arc::new(
                values
                    .into_iter()
                    .map(DataValue::decode)
                    .collect::<Result<_, _>>()
                    .map_err(serde::de::Error::custom)?,
            )),
            ResultKeyImage::InputHash(hash) => Self::InputHash(hash),
            ResultKeyImage::Element(reference) => Self::Element(reference),
        })
    }
}

impl PartialEq for ResultKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::GroupBy(left), Self::GroupBy(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right.iter())
                        .all(|(a, b)| a.eq_for_groupby(b))
            }
            (Self::InputHash(left), Self::InputHash(right)) => left == right,
            (Self::Element(left), Self::Element(right)) => left == right,
            _ => false,
        }
    }
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
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    fn hash(key: &ResultKey) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn result_keys_keep_exact_numeric_grouping_identity() {
        let key = |value| ResultKey::GroupBy(Arc::new(vec![value]));
        let integer = key(VariableValue::from(serde_json::json!(
            9_007_199_254_740_992_i64
        )));
        let float = key(VariableValue::from(serde_json::json!(
            9_007_199_254_740_992.0
        )));
        let next = key(VariableValue::from(serde_json::json!(
            9_007_199_254_740_993_i64
        )));
        assert_eq!(integer, float);
        assert_eq!(hash(&integer), hash(&float));
        assert_ne!(integer, next);
    }

    #[test]
    fn result_key_messagepack_preserves_grouping_values_and_hash() {
        let images = [
            DataValue::Unsigned(9_007_199_254_740_993),
            DataValue::Float(0x7ff8_0000_0000_0042),
            DataValue::Float((-0.0_f64).to_bits()),
            DataValue::Range(None, Some(-1)),
        ];
        let key = ResultKey::GroupBy(Arc::new(
            images
                .iter()
                .cloned()
                .map(|value| value.decode().expect("valid value"))
                .collect(),
        ));
        let bytes = rmp_serde::to_vec_named(&key).expect("encode key");
        let decoded: ResultKey = rmp_serde::from_slice(&bytes).expect("decode key");
        assert_eq!(hash(&key), hash(&decoded));
        let ResultKey::GroupBy(values) = decoded else {
            panic!("expected group key");
        };
        assert_eq!(
            values
                .iter()
                .map(DataValue::encode)
                .collect::<Result<Vec<_>, _>>()
                .expect("encode values"),
            images
        );
    }

    #[test]
    fn accumulator_messagepack_preserves_float_bits() {
        let bits = 0x7ff8_0000_0000_0042;
        let value = ValueAccumulator::Sum {
            value: f64::from_bits(bits),
        };
        let bytes = rmp_serde::to_vec_named(&value).expect("encode accumulator");
        let decoded: ValueAccumulator = rmp_serde::from_slice(&bytes).expect("decode accumulator");
        let ValueAccumulator::Sum { value } = decoded else {
            panic!("expected sum");
        };
        assert_eq!(value.to_bits(), bits);
    }
}
