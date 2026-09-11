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

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use drasi_query_ast::ast::Expression;
use ordered_float::OrderedFloat;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    evaluation::{
        context::QueryVariables,
        variable_value::{
            duration::Duration, float::Float, integer::Integer, zoned_datetime::ZonedDateTime,
            zoned_time::ZonedTime, ListRange, RangeBound, VariableValue,
        },
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue},
};

use super::codec::TemporalCodecError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum StoredValue {
    Null,
    Bool(bool),
    FloatBits(u64),
    Unsigned(u64),
    Negative(i64),
    String(String),
    List(Vec<StoredValue>),
    Object(BTreeMap<String, StoredValue>),
    Date(i32),
    LocalTime(StoredTime),
    ZonedTime {
        time: StoredTime,
        offset: i32,
    },
    LocalDateTime(StoredDateTime),
    ZonedDateTime {
        datetime: StoredZonedDateTime,
        zone: Option<String>,
    },
    Duration {
        seconds: i64,
        nanos: i32,
        years: i64,
        months: i64,
    },
    Expression(Expression),
    ListRange {
        start: Option<i64>,
        end: Option<i64>,
    },
    Element(StoredElement),
    ElementMetadata(ElementMetadata),
    ElementReference(ElementReference),
    Awaiting,
}

#[derive(Serialize)]
pub(super) enum StoredGroupingValue {
    Element(ElementReference),
    Value(StoredValue),
}

impl TryFrom<&VariableValue> for StoredGroupingValue {
    type Error = TemporalCodecError;

    fn try_from(value: &VariableValue) -> Result<Self, Self::Error> {
        // Match hash_for_groupby: a directly grouped element is identified by reference.
        if let VariableValue::Element(element) = value {
            return Ok(Self::Element(element.get_reference().clone()));
        }
        let mut stored = StoredValue::try_from(value)?;
        stored.normalize_group_identity();
        Ok(Self::Value(stored))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredTime {
    seconds: u32,
    nanos: u32,
}

impl From<NaiveTime> for StoredTime {
    fn from(value: NaiveTime) -> Self {
        Self {
            seconds: value.num_seconds_from_midnight(),
            nanos: value.nanosecond(),
        }
    }
}

impl StoredTime {
    fn restore(self) -> Result<NaiveTime, TemporalCodecError> {
        NaiveTime::from_num_seconds_from_midnight_opt(self.seconds, self.nanos)
            .ok_or_else(|| TemporalCodecError::invalid("invalid time or leap second"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredDateTime {
    days: i32,
    time: StoredTime,
}

impl From<NaiveDateTime> for StoredDateTime {
    fn from(value: NaiveDateTime) -> Self {
        Self {
            days: value.date().num_days_from_ce(),
            time: value.time().into(),
        }
    }
}

impl StoredDateTime {
    fn restore(self) -> Result<NaiveDateTime, TemporalCodecError> {
        let date = restore_date(self.days)?;
        Ok(date.and_time(self.time.restore()?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredZonedDateTime {
    utc: StoredDateTime,
    offset: i32,
}

impl From<DateTime<FixedOffset>> for StoredZonedDateTime {
    fn from(value: DateTime<FixedOffset>) -> Self {
        Self {
            utc: value.naive_utc().into(),
            offset: value.offset().local_minus_utc(),
        }
    }
}

impl StoredZonedDateTime {
    fn restore(self) -> Result<DateTime<FixedOffset>, TemporalCodecError> {
        Ok(DateTime::from_naive_utc_and_offset(
            self.utc.restore()?,
            restore_offset(self.offset)?,
        ))
    }
}

fn restore_date(days: i32) -> Result<NaiveDate, TemporalCodecError> {
    NaiveDate::from_num_days_from_ce_opt(days)
        .ok_or_else(|| TemporalCodecError::invalid("invalid date"))
}

fn restore_offset(seconds: i32) -> Result<FixedOffset, TemporalCodecError> {
    FixedOffset::east_opt(seconds).ok_or_else(|| TemporalCodecError::invalid("invalid UTC offset"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum StoredProperty {
    Null,
    Bool(bool),
    FloatBits(u64),
    Integer(i64),
    String(Arc<str>),
    List(Vec<StoredProperty>),
    Object(BTreeMap<String, StoredProperty>),
    LocalDateTime(StoredDateTime),
    ZonedDateTime(StoredZonedDateTime),
}

impl From<&ElementValue> for StoredProperty {
    fn from(value: &ElementValue) -> Self {
        match value {
            ElementValue::Null => Self::Null,
            ElementValue::Bool(v) => Self::Bool(*v),
            ElementValue::Float(v) => Self::FloatBits(v.0.to_bits()),
            ElementValue::Integer(v) => Self::Integer(*v),
            ElementValue::String(v) => Self::String(v.clone()),
            ElementValue::List(v) => Self::List(v.iter().map(Self::from).collect()),
            ElementValue::Object(v) => Self::Object(store_properties(v)),
            ElementValue::LocalDateTime(v) => Self::LocalDateTime((*v).into()),
            ElementValue::ZonedDateTime(v) => Self::ZonedDateTime((*v).into()),
        }
    }
}

impl StoredProperty {
    fn normalize_group_identity(&mut self) {
        match self {
            Self::FloatBits(bits) => {
                let value = f64::from_bits(*bits);
                if value == 0.0 {
                    *bits = 0;
                } else if value.is_nan() {
                    *bits = f64::NAN.to_bits();
                }
            }
            Self::List(values) => values.iter_mut().for_each(Self::normalize_group_identity),
            Self::Object(values) => values.values_mut().for_each(Self::normalize_group_identity),
            Self::ZonedDateTime(value) => value.offset = 0,
            Self::Null
            | Self::Bool(_)
            | Self::Integer(_)
            | Self::String(_)
            | Self::LocalDateTime(_) => {}
        }
    }

    fn restore(self) -> Result<ElementValue, TemporalCodecError> {
        Ok(match self {
            Self::Null => ElementValue::Null,
            Self::Bool(v) => ElementValue::Bool(v),
            Self::FloatBits(v) => ElementValue::Float(OrderedFloat(f64::from_bits(v))),
            Self::Integer(v) => ElementValue::Integer(v),
            Self::String(v) => ElementValue::String(v),
            Self::List(v) => {
                ElementValue::List(v.into_iter().map(Self::restore).collect::<Result<_, _>>()?)
            }
            Self::Object(v) => ElementValue::Object(restore_properties(v)?),
            Self::LocalDateTime(v) => ElementValue::LocalDateTime(v.restore()?),
            Self::ZonedDateTime(v) => ElementValue::ZonedDateTime(v.restore()?),
        })
    }
}

fn store_properties(value: &ElementPropertyMap) -> BTreeMap<String, StoredProperty> {
    value
        .map_iter(|key, value| (key.to_string(), StoredProperty::from(value)))
        .collect()
}

fn restore_properties(
    values: BTreeMap<String, StoredProperty>,
) -> Result<ElementPropertyMap, TemporalCodecError> {
    let values: BTreeMap<String, ElementValue> = values
        .into_iter()
        .map(|(key, value)| Ok((key, value.restore()?)))
        .collect::<Result<_, TemporalCodecError>>()?;
    Ok(values.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum StoredElement {
    Node {
        metadata: ElementMetadata,
        properties: BTreeMap<String, StoredProperty>,
    },
    Relation {
        metadata: ElementMetadata,
        properties: BTreeMap<String, StoredProperty>,
        in_node: ElementReference,
        out_node: ElementReference,
    },
}

impl From<&Element> for StoredElement {
    fn from(value: &Element) -> Self {
        match value {
            Element::Node {
                metadata,
                properties,
            } => Self::Node {
                metadata: metadata.clone(),
                properties: store_properties(properties),
            },
            Element::Relation {
                metadata,
                properties,
                in_node,
                out_node,
            } => Self::Relation {
                metadata: metadata.clone(),
                properties: store_properties(properties),
                in_node: in_node.clone(),
                out_node: out_node.clone(),
            },
        }
    }
}

impl StoredElement {
    fn normalize_group_identity(&mut self) {
        let properties = match self {
            Self::Node { properties, .. } | Self::Relation { properties, .. } => properties,
        };
        properties
            .values_mut()
            .for_each(StoredProperty::normalize_group_identity);
    }

    fn restore(self) -> Result<Element, TemporalCodecError> {
        Ok(match self {
            Self::Node {
                metadata,
                properties,
            } => Element::Node {
                metadata,
                properties: restore_properties(properties)?,
            },
            Self::Relation {
                metadata,
                properties,
                in_node,
                out_node,
            } => Element::Relation {
                metadata,
                properties: restore_properties(properties)?,
                in_node,
                out_node,
            },
        })
    }
}

impl TryFrom<&VariableValue> for StoredValue {
    type Error = TemporalCodecError;

    fn try_from(value: &VariableValue) -> Result<Self, Self::Error> {
        Ok(match value {
            VariableValue::Null => Self::Null,
            VariableValue::Bool(v) => Self::Bool(*v),
            VariableValue::Float(v) => Self::FloatBits(v.to_bits()),
            VariableValue::Integer(v) => match (v.as_u64(), v.as_i64()) {
                (Some(v), _) => Self::Unsigned(v),
                (None, Some(v)) if v < 0 => Self::Negative(v),
                _ => {
                    return Err(TemporalCodecError::invalid(
                        "invalid integer representation",
                    ))
                }
            },
            VariableValue::String(v) => Self::String(v.clone()),
            VariableValue::List(v) => {
                Self::List(v.iter().map(Self::try_from).collect::<Result<_, _>>()?)
            }
            VariableValue::Object(v) => Self::Object(
                v.iter()
                    .map(|(key, value)| Ok((key.clone(), Self::try_from(value)?)))
                    .collect::<Result<_, TemporalCodecError>>()?,
            ),
            VariableValue::Date(v) => Self::Date(v.num_days_from_ce()),
            VariableValue::LocalTime(v) => Self::LocalTime((*v).into()),
            VariableValue::ZonedTime(v) => Self::ZonedTime {
                time: (*v.time()).into(),
                offset: v.offset().local_minus_utc(),
            },
            VariableValue::LocalDateTime(v) => Self::LocalDateTime((*v).into()),
            VariableValue::ZonedDateTime(v) => Self::ZonedDateTime {
                datetime: (*v.datetime()).into(),
                zone: v.timezone_name().clone(),
            },
            VariableValue::Duration(v) => Self::Duration {
                seconds: v.duration().num_seconds(),
                nanos: v.duration().subsec_nanos(),
                years: *v.year(),
                months: *v.month(),
            },
            VariableValue::Expression(v) => Self::Expression(v.clone()),
            VariableValue::ListRange(v) => Self::ListRange {
                start: store_bound(&v.start),
                end: store_bound(&v.end),
            },
            VariableValue::Element(v) => Self::Element(v.as_ref().into()),
            VariableValue::ElementMetadata(v) => Self::ElementMetadata(v.clone()),
            VariableValue::ElementReference(v) => Self::ElementReference(v.clone()),
            VariableValue::Awaiting => Self::Awaiting,
        })
    }
}

fn store_bound(bound: &RangeBound) -> Option<i64> {
    match bound {
        RangeBound::Index(v) => Some(*v),
        RangeBound::Unbounded => None,
    }
}

impl StoredValue {
    fn normalize_group_identity(&mut self) {
        match self {
            Self::FloatBits(bits) => {
                if f64::from_bits(*bits) == 0.0 {
                    *bits = 0;
                }
            }
            Self::List(values) => values.iter_mut().for_each(Self::normalize_group_identity),
            Self::Object(values) => values.values_mut().for_each(Self::normalize_group_identity),
            Self::ZonedDateTime { datetime, .. } => datetime.offset = 0,
            Self::Element(value) => value.normalize_group_identity(),
            Self::Null
            | Self::Bool(_)
            | Self::Unsigned(_)
            | Self::Negative(_)
            | Self::String(_)
            | Self::Date(_)
            | Self::LocalTime(_)
            | Self::ZonedTime { .. }
            | Self::LocalDateTime(_)
            | Self::Duration { .. }
            | Self::Expression(_)
            | Self::ListRange { .. }
            | Self::ElementMetadata(_)
            | Self::ElementReference(_)
            | Self::Awaiting => {}
        }
    }

    pub(super) fn restore(self) -> Result<VariableValue, TemporalCodecError> {
        Ok(match self {
            Self::Null => VariableValue::Null,
            Self::Bool(v) => VariableValue::Bool(v),
            Self::FloatBits(v) => VariableValue::Float(Float::from(f64::from_bits(v))),
            Self::Unsigned(v) => VariableValue::Integer(Integer::from(v)),
            Self::Negative(v) => {
                if v >= 0 {
                    return Err(TemporalCodecError::invalid(
                        "negative integer is not negative",
                    ));
                }
                VariableValue::Integer(Integer::from(v))
            }
            Self::String(v) => VariableValue::String(v),
            Self::List(v) => {
                VariableValue::List(v.into_iter().map(Self::restore).collect::<Result<_, _>>()?)
            }
            Self::Object(v) => VariableValue::Object(
                v.into_iter()
                    .map(|(key, value)| Ok((key, value.restore()?)))
                    .collect::<Result<_, TemporalCodecError>>()?,
            ),
            Self::Date(v) => VariableValue::Date(restore_date(v)?),
            Self::LocalTime(v) => VariableValue::LocalTime(v.restore()?),
            Self::ZonedTime { time, offset } => {
                VariableValue::ZonedTime(ZonedTime::new(time.restore()?, restore_offset(offset)?))
            }
            Self::LocalDateTime(v) => VariableValue::LocalDateTime(v.restore()?),
            Self::ZonedDateTime { datetime, zone } => {
                VariableValue::ZonedDateTime(ZonedDateTime::new(datetime.restore()?, zone))
            }
            Self::Duration {
                seconds,
                nanos,
                years,
                months,
            } => {
                if !(-999_999_999..=999_999_999).contains(&nanos)
                    || (seconds > 0 && nanos < 0)
                    || (seconds < 0 && nanos > 0)
                {
                    return Err(TemporalCodecError::invalid("invalid duration components"));
                }
                let duration = chrono::Duration::try_seconds(seconds)
                    .and_then(|d| d.checked_add(&chrono::Duration::nanoseconds(i64::from(nanos))))
                    .ok_or_else(|| TemporalCodecError::invalid("duration out of range"))?;
                VariableValue::Duration(Duration::new(duration, years, months))
            }
            Self::Expression(v) => VariableValue::Expression(v),
            Self::ListRange { start, end } => VariableValue::ListRange(ListRange {
                start: start.map_or(RangeBound::Unbounded, RangeBound::Index),
                end: end.map_or(RangeBound::Unbounded, RangeBound::Index),
            }),
            Self::Element(v) => VariableValue::Element(Arc::new(v.restore()?)),
            Self::ElementMetadata(v) => VariableValue::ElementMetadata(v),
            Self::ElementReference(v) => VariableValue::ElementReference(v),
            Self::Awaiting => VariableValue::Awaiting,
        })
    }
}

pub(super) fn serialize<S: Serializer>(
    value: &VariableValue,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    StoredValue::try_from(value)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<VariableValue, D::Error> {
    StoredValue::deserialize(deserializer)?
        .restore()
        .map_err(serde::de::Error::custom)
}

pub(super) mod variables {
    use super::*;

    pub fn serialize<S: Serializer>(values: &QueryVariables, s: S) -> Result<S::Ok, S::Error> {
        let stored: BTreeMap<&str, StoredValue> = values
            .iter()
            .map(|(key, value)| Ok((key.as_ref(), StoredValue::try_from(value)?)))
            .collect::<Result<_, TemporalCodecError>>()
            .map_err(serde::ser::Error::custom)?;
        stored.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<QueryVariables, D::Error> {
        BTreeMap::<Box<str>, StoredValue>::deserialize(d)?
            .into_iter()
            .map(|(key, value)| Ok((key, value.restore().map_err(serde::de::Error::custom)?)))
            .collect()
    }
}

pub(super) mod values {
    use super::*;

    pub fn serialize<S: Serializer>(values: &[VariableValue], s: S) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(StoredValue::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<VariableValue>, D::Error> {
        Vec::<StoredValue>::deserialize(d)?
            .into_iter()
            .map(|v| v.restore().map_err(serde::de::Error::custom))
            .collect()
    }
}

pub(super) mod optional_element {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Option<Arc<Element>>, s: S) -> Result<S::Ok, S::Error> {
        value.as_deref().map(StoredElement::from).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Arc<Element>>, D::Error> {
        Option::<StoredElement>::deserialize(d)?
            .map(|v| v.restore().map(Arc::new).map_err(serde::de::Error::custom))
            .transpose()
    }
}
