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

use drasi_core::evaluation::functions::aggregation::ValueAccumulator;
use prost::bytes::{Bytes, BytesMut};

use crate::storage_models::StoredValueMap;

#[derive(PartialEq, ::prost::Message)]
pub struct StoredValueAccumulatorContainer {
    #[prost(oneof = "StoredValueAccumulator", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub value: ::core::option::Option<StoredValueAccumulator>,
}

#[derive(PartialEq, ::prost::Oneof)]
pub enum StoredValueAccumulator {
    #[prost(double, tag = "1")]
    Sum(f64),

    #[prost(message, tag = "2")]
    Avg(StoredAverage),

    #[prost(int64, tag = "3")]
    Count(i64),

    #[prost(uint64, tag = "4")]
    TimeMarker(u64),

    #[prost(uint64, tag = "5")]
    Signature(u64),

    #[prost(message, tag = "6")]
    LinearGradient(StoredLinearGradient),

    #[prost(message, tag = "7")]
    Value(super::stored_value::StoredValueContainer),

    #[prost(message, tag = "8")]
    Map(StoredValueMap),
}

impl StoredValueAccumulator {
    pub fn serialize(&self) -> Bytes {
        let mut buffer = BytesMut::new();
        self.encode(&mut buffer);
        buffer.freeze()
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StoredAverage {
    #[prost(double, tag = "1")]
    pub sum: f64,
    #[prost(int64, tag = "2")]
    pub count: i64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StoredLinearGradient {
    #[prost(int64, tag = "1")]
    pub count: i64,
    #[prost(double, tag = "2")]
    pub mean_x: f64,
    #[prost(double, tag = "3")]
    pub mean_y: f64,
    #[prost(double, tag = "4")]
    pub m2: f64,
    #[prost(double, tag = "5")]
    pub cov: f64,
}

impl From<ValueAccumulator> for StoredValueAccumulator {
    fn from(acc: ValueAccumulator) -> Self {
        match acc {
            ValueAccumulator::Sum { value } => StoredValueAccumulator::Sum(value),
            ValueAccumulator::Avg { sum, count } => {
                StoredValueAccumulator::Avg(StoredAverage { sum, count })
            }
            ValueAccumulator::Count { value } => StoredValueAccumulator::Count(value),
            ValueAccumulator::TimeMarker { timestamp } => {
                StoredValueAccumulator::TimeMarker(timestamp)
            }
            ValueAccumulator::Signature(signature) => StoredValueAccumulator::Signature(signature),
            ValueAccumulator::LinearGradient {
                count,
                mean_x,
                mean_y,
                m2,
                cov,
            } => StoredValueAccumulator::LinearGradient(StoredLinearGradient {
                count,
                mean_x,
                mean_y,
                m2,
                cov,
            }),
            ValueAccumulator::Value(value) => StoredValueAccumulator::Value((&value).into()),
            ValueAccumulator::Map(map) => StoredValueAccumulator::Map((&map).into()),
        }
    }
}

impl From<StoredValueAccumulator> for ValueAccumulator {
    fn from(val: StoredValueAccumulator) -> Self {
        match val {
            StoredValueAccumulator::Sum(value) => ValueAccumulator::Sum { value },
            StoredValueAccumulator::Avg(avg) => ValueAccumulator::Avg {
                sum: avg.sum,
                count: avg.count,
            },
            StoredValueAccumulator::Count(value) => ValueAccumulator::Count { value },
            StoredValueAccumulator::TimeMarker(timestamp) => {
                ValueAccumulator::TimeMarker { timestamp }
            }
            StoredValueAccumulator::Signature(signature) => ValueAccumulator::Signature(signature),
            StoredValueAccumulator::LinearGradient(linear_gradient) => {
                ValueAccumulator::LinearGradient {
                    count: linear_gradient.count,
                    mean_x: linear_gradient.mean_x,
                    mean_y: linear_gradient.mean_y,
                    m2: linear_gradient.m2,
                    cov: linear_gradient.cov,
                }
            }
            StoredValueAccumulator::Value(value) => ValueAccumulator::Value(value.into()),
            StoredValueAccumulator::Map(map) => ValueAccumulator::Map(map.into()),
        }
    }
}
