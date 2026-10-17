// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only query-resource protocol. This is not a production Query ABI.

use anyhow::{ensure, Result};
use drasi_computation_contracts::QueryValueCodec;
use drasi_core::{
    evaluation::{functions::aggregation::ValueAccumulator, variable_value::VariableValue},
    interface::{IndexError, ResultKey, ResultOwner},
    models::{Element, ElementPropertyMap, ElementReference, ElementValue},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const LIMIT: usize = 1024 * 1024;
pub const SCAN_LIMIT: usize = 128;
pub const RESOURCE_REQUEST: u32 = 1;
pub const ENTRY: &[u8] = b"drasi_native_query_proof\0";

pub fn index_error(error: anyhow::Error) -> IndexError {
    IndexError::Other(error.into_boxed_dyn_error())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub query: String,
}

#[derive(Serialize, Deserialize)]
pub enum Key {
    Group(serde_bytes::ByteBuf),
    Input(u64),
    Element(ElementReference),
}

impl Key {
    pub fn encode(value: &ResultKey) -> Result<Self> {
        Ok(match value {
            ResultKey::GroupBy(values) => Self::Group(
                QueryValueCodec::encode(&VariableValue::List(values.as_ref().clone()), LIMIT)?
                    .to_vec()
                    .into(),
            ),
            ResultKey::InputHash(value) => Self::Input(*value),
            ResultKey::Element(value) => Self::Element(value.clone()),
        })
    }

    pub fn decode(self) -> Result<ResultKey> {
        Ok(match self {
            Self::Group(value) => match QueryValueCodec::decode(&value, LIMIT)? {
                VariableValue::List(values) => ResultKey::GroupBy(Arc::new(values)),
                _ => anyhow::bail!("query index grouping key is not a list"),
            },
            Self::Input(value) => ResultKey::InputHash(value),
            Self::Element(value) => ResultKey::Element(value),
        })
    }
}

#[derive(Serialize, Deserialize)]
pub enum Owner {
    Function(usize),
    Current(usize),
    Default(usize),
    TransactionTime(usize),
    Realtime(usize),
    Tombstone,
}

impl From<&ResultOwner> for Owner {
    fn from(value: &ResultOwner) -> Self {
        match value {
            ResultOwner::Function(value) => Self::Function(*value),
            ResultOwner::PartCurrent(value) => Self::Current(*value),
            ResultOwner::PartDefault(value) => Self::Default(*value),
            ResultOwner::PartTransactionTime(value) => Self::TransactionTime(*value),
            ResultOwner::PartRealtime(value) => Self::Realtime(*value),
            ResultOwner::ElementTombstone => Self::Tombstone,
        }
    }
}

impl From<Owner> for ResultOwner {
    fn from(value: Owner) -> Self {
        match value {
            Owner::Function(value) => Self::Function(value),
            Owner::Current(value) => Self::PartCurrent(value),
            Owner::Default(value) => Self::PartDefault(value),
            Owner::TransactionTime(value) => Self::PartTransactionTime(value),
            Owner::Realtime(value) => Self::PartRealtime(value),
            Owner::Tombstone => Self::ElementTombstone,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum Accumulator {
    Sum(u64),
    Avg(u64, i64),
    Count(i64),
    Time(u64),
    Signature(u64),
    Gradient(i64, u64, u64, u64, u64),
    Value(ElementValue),
    Map(ElementPropertyMap),
}

impl From<ValueAccumulator> for Accumulator {
    fn from(value: ValueAccumulator) -> Self {
        match value {
            ValueAccumulator::Sum { value } => Self::Sum(value.to_bits()),
            ValueAccumulator::Avg { sum, count } => Self::Avg(sum.to_bits(), count),
            ValueAccumulator::Count { value } => Self::Count(value),
            ValueAccumulator::TimeMarker { timestamp } => Self::Time(timestamp),
            ValueAccumulator::Signature(value) => Self::Signature(value),
            ValueAccumulator::LinearGradient {
                count,
                mean_x,
                mean_y,
                m2,
                cov,
            } => Self::Gradient(
                count,
                mean_x.to_bits(),
                mean_y.to_bits(),
                m2.to_bits(),
                cov.to_bits(),
            ),
            ValueAccumulator::Value(value) => Self::Value(value),
            ValueAccumulator::Map(value) => Self::Map(value),
        }
    }
}

impl From<Accumulator> for ValueAccumulator {
    fn from(value: Accumulator) -> Self {
        match value {
            Accumulator::Sum(value) => Self::Sum {
                value: f64::from_bits(value),
            },
            Accumulator::Avg(sum, count) => Self::Avg {
                sum: f64::from_bits(sum),
                count,
            },
            Accumulator::Count(value) => Self::Count { value },
            Accumulator::Time(timestamp) => Self::TimeMarker { timestamp },
            Accumulator::Signature(value) => Self::Signature(value),
            Accumulator::Gradient(count, mean_x, mean_y, m2, cov) => Self::LinearGradient {
                count,
                mean_x: f64::from_bits(mean_x),
                mean_y: f64::from_bits(mean_y),
                m2: f64::from_bits(m2),
                cov: f64::from_bits(cov),
            },
            Accumulator::Value(value) => Self::Value(value),
            Accumulator::Map(value) => Self::Map(value),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum Request {
    GetElement(ElementReference),
    SetElement(Element, Vec<usize>),
    DeleteElement(ElementReference),
    GetSlot(usize, ElementReference),
    ScanSlot(usize),
    ScanInbound(usize, ElementReference),
    ScanOutbound(usize, ElementReference),
    Configure { virtual_joins: bool },
    GetResult(Key, Owner),
    SetResult(Key, Owner, Option<Accumulator>),
    NextValue(u64, Option<u64>),
    ValueCount(u64, u64),
    IncrementCount(u64, u64, i64),
    GetSequence,
    ApplySequence(u64, String),
    PushFuture(u8, usize, u64, ElementReference, u64, u64),
    RemoveFuture(usize, u64),
    PopFuture,
    PeekFuture,
    Clear(u8),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(drasi_computation_contracts::encode_bounded_messagepack(
        value, LIMIT,
    )?)
}

pub fn decode<'a, T: Deserialize<'a>>(value: &'a [u8]) -> Result<T> {
    ensure!(
        value.len() <= LIMIT,
        "query proof message exceeds its limit"
    );
    Ok(drasi_computation_contracts::decode_bounded_messagepack(
        value, LIMIT,
    )?)
}
