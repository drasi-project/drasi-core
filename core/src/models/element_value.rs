#![allow(clippy::unwrap_used)]
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

use super::ConversionError;

use std::{
    collections::BTreeMap,
    ops::{Index, IndexMut},
};

use crate::evaluation::variable_value::{
    float::Float, integer::Integer, zoned_datetime::ZonedDateTime as VarZonedDateTime,
    VariableValue,
};

use std::sync::Arc;

use chrono::{DateTime, FixedOffset, NaiveDateTime};
use ordered_float::OrderedFloat;

#[derive(Debug, Clone, Hash, Eq, PartialEq, Default)]
pub enum ElementValue {
    #[default]
    Null,
    Bool(bool),
    Float(OrderedFloat<f64>),
    Integer(i64),
    String(Arc<str>),
    List(Vec<ElementValue>),
    Object(ElementPropertyMap),
    LocalDateTime(NaiveDateTime),
    ZonedDateTime(DateTime<FixedOffset>),
}

impl From<&ElementPropertyMap> for VariableValue {
    fn from(val: &ElementPropertyMap) -> Self {
        let mut map = BTreeMap::new();
        for (key, value) in val.values.iter() {
            map.insert(key.to_string(), value.into());
        }
        VariableValue::Object(map)
    }
}

impl From<&ElementValue> for VariableValue {
    fn from(val: &ElementValue) -> Self {
        match val {
            ElementValue::Null => VariableValue::Null,
            ElementValue::Bool(b) => VariableValue::Bool(*b),
            ElementValue::Float(f) => VariableValue::Float(Float::from(f.0)),
            ElementValue::Integer(i) => VariableValue::Integer(Integer::from(*i)),
            ElementValue::String(s) => VariableValue::String(s.to_string()),
            ElementValue::List(l) => VariableValue::List(l.iter().map(|x| x.into()).collect()),
            ElementValue::Object(o) => o.into(),
            ElementValue::LocalDateTime(dt) => VariableValue::LocalDateTime(*dt),
            ElementValue::ZonedDateTime(dt) => {
                VariableValue::ZonedDateTime(VarZonedDateTime::new(*dt, None))
            }
        }
    }
}

impl TryInto<ElementValue> for &VariableValue {
    type Error = ConversionError;

    fn try_into(self) -> Result<ElementValue, ConversionError> {
        match self {
            VariableValue::Null => Ok(ElementValue::Null),
            VariableValue::Bool(b) => Ok(ElementValue::Bool(*b)),
            VariableValue::Float(f) => Ok(ElementValue::Float(OrderedFloat(
                f.as_f64().unwrap_or_default(),
            ))),
            VariableValue::Integer(i) => Ok(ElementValue::Integer(i.as_i64().unwrap_or_default())),
            VariableValue::String(s) => Ok(ElementValue::String(Arc::from(s.as_str()))),
            VariableValue::List(l) => Ok(ElementValue::List(
                l.iter().map(|x| x.try_into().unwrap_or_default()).collect(),
            )),
            VariableValue::Object(o) => Ok(ElementValue::Object(o.into())),
            VariableValue::LocalDateTime(dt) => Ok(ElementValue::LocalDateTime(*dt)),
            VariableValue::ZonedDateTime(zdt) => Ok(ElementValue::ZonedDateTime(*zdt.datetime())),
            _ => Err(ConversionError {}),
        }
    }
}

impl TryInto<ElementValue> for VariableValue {
    type Error = ConversionError;

    fn try_into(self) -> Result<ElementValue, ConversionError> {
        match self {
            VariableValue::Null => Ok(ElementValue::Null),
            VariableValue::Bool(b) => Ok(ElementValue::Bool(b)),
            VariableValue::Float(f) => Ok(ElementValue::Float(OrderedFloat(
                f.as_f64().unwrap_or_default(),
            ))),
            VariableValue::Integer(i) => Ok(ElementValue::Integer(i.as_i64().unwrap_or_default())),
            VariableValue::String(s) => Ok(ElementValue::String(Arc::from(s.as_str()))),
            VariableValue::List(l) => Ok(ElementValue::List(
                l.iter().map(|x| x.try_into().unwrap_or_default()).collect(),
            )),
            VariableValue::Object(o) => Ok(ElementValue::Object(o.into())),
            VariableValue::LocalDateTime(dt) => Ok(ElementValue::LocalDateTime(dt)),
            VariableValue::ZonedDateTime(zdt) => Ok(ElementValue::ZonedDateTime(*zdt.datetime())),
            _ => Err(ConversionError {}),
        }
    }
}

impl From<&ElementValue> for serde_json::Value {
    fn from(val: &ElementValue) -> Self {
        match val {
            ElementValue::Null => serde_json::Value::Null,
            ElementValue::Bool(b) => serde_json::Value::Bool(*b),
            ElementValue::Float(f) => {
                serde_json::Value::Number(serde_json::Number::from_f64(f.into_inner()).unwrap())
            }
            ElementValue::Integer(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
            ElementValue::String(s) => serde_json::Value::String(s.to_string()),
            ElementValue::List(l) => serde_json::Value::Array(l.iter().map(|x| x.into()).collect()),
            ElementValue::Object(o) => serde_json::Value::Object(o.into()),
            ElementValue::LocalDateTime(dt) => serde_json::Value::String(dt.to_string()),
            ElementValue::ZonedDateTime(dt) => serde_json::Value::String(dt.to_rfc3339()),
        }
    }
}

impl From<&serde_json::Value> for ElementValue {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => ElementValue::Null,
            serde_json::Value::Bool(b) => ElementValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    ElementValue::Integer(i)
                } else if let Some(f) = n.as_f64() {
                    ElementValue::Float(OrderedFloat(f))
                } else {
                    ElementValue::Null
                }
            }
            serde_json::Value::String(s) => ElementValue::String(Arc::from(s.as_str())),
            serde_json::Value::Array(a) => ElementValue::List(a.iter().map(|x| x.into()).collect()),
            serde_json::Value::Object(o) => ElementValue::Object(o.into()),
        }
    }
}

impl TryInto<ElementPropertyMap> for &VariableValue {
    type Error = ConversionError;

    fn try_into(self) -> Result<ElementPropertyMap, ConversionError> {
        match self {
            VariableValue::Object(o) => {
                let mut values = BTreeMap::new();
                for (key, value) in o.iter() {
                    values.insert(Arc::from(key.as_str()), value.try_into()?);
                }
                Ok(ElementPropertyMap { values })
            }
            _ => Err(ConversionError {}),
        }
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ElementPropertyMap {
    values: BTreeMap<Arc<str>, ElementValue>,
}

impl Default for ElementPropertyMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ElementPropertyMap {
    pub fn new() -> Self {
        ElementPropertyMap {
            values: BTreeMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&ElementValue> {
        self.values.get(key)
    }

    pub fn insert(&mut self, key: &str, value: ElementValue) {
        self.values.insert(Arc::from(key), value);
    }

    pub fn merge(&mut self, other: &ElementPropertyMap) {
        for (key, value) in other.values.iter() {
            self.values
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }

    pub fn map_iter<T>(
        &self,
        f: impl Fn(&Arc<str>, &ElementValue) -> T + 'static,
    ) -> impl Iterator<Item = T> + '_ {
        self.values.iter().map(move |(k, v)| f(k, v))
    }
}

impl Index<&str> for ElementPropertyMap {
    type Output = ElementValue;

    fn index(&self, key: &str) -> &Self::Output {
        static NULL: ElementValue = ElementValue::Null;
        match self.values.get(key) {
            Some(value) => value,
            None => &NULL,
        }
    }
}

impl IndexMut<&str> for ElementPropertyMap {
    fn index_mut(&mut self, key: &str) -> &mut Self::Output {
        self.values
            .entry(Arc::from(key))
            .or_insert_with(|| ElementValue::Null)
    }
}

impl From<&BTreeMap<String, VariableValue>> for ElementPropertyMap {
    fn from(map: &BTreeMap<String, VariableValue>) -> Self {
        let mut values = BTreeMap::new();
        for (key, value) in map.iter() {
            values.insert(Arc::from(key.as_str()), value.try_into().unwrap());
        }
        ElementPropertyMap { values }
    }
}

impl From<BTreeMap<String, VariableValue>> for ElementPropertyMap {
    fn from(map: BTreeMap<String, VariableValue>) -> Self {
        let mut values = BTreeMap::new();
        for (key, value) in map {
            values.insert(Arc::from(key.as_str()), value.try_into().unwrap());
        }
        ElementPropertyMap { values }
    }
}

impl From<BTreeMap<String, ElementValue>> for ElementPropertyMap {
    fn from(map: BTreeMap<String, ElementValue>) -> Self {
        let mut values = BTreeMap::new();
        for (key, value) in map {
            values.insert(Arc::from(key.as_str()), value);
        }
        ElementPropertyMap { values }
    }
}

impl From<&ElementPropertyMap> for serde_json::Map<String, serde_json::Value> {
    fn from(val: &ElementPropertyMap) -> Self {
        val.values
            .iter()
            .map(|(k, v)| (k.to_string(), v.into()))
            .collect()
    }
}

impl From<&serde_json::Map<String, serde_json::Value>> for ElementPropertyMap {
    fn from(map: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut values = BTreeMap::new();
        for (key, value) in map.iter() {
            values.insert(Arc::from(key.as_str()), value.into());
        }
        ElementPropertyMap { values }
    }
}

impl From<serde_json::Value> for ElementPropertyMap {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Object(o) => (&o).into(),
            _ => ElementPropertyMap::new(),
        }
    }
}

impl From<&serde_json::Value> for ElementPropertyMap {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Object(o) => o.into(),
            _ => ElementPropertyMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::variable_value::{
        float::Float, integer::Integer, zoned_datetime::ZonedDateTime as VarZonedDateTime,
        VariableValue,
    };
    use chrono::{NaiveDate, TimeZone, Utc};

    fn sample_naive_datetime() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 45)
            .unwrap()
    }

    fn sample_fixed_datetime() -> DateTime<FixedOffset> {
        let offset = FixedOffset::east_opt(3600).unwrap(); // +01:00
        offset.with_ymd_and_hms(2024, 6, 15, 10, 30, 45).unwrap()
    }

    // ── ElementValue → VariableValue ────────────────────────────────────

    #[test]
    fn local_datetime_to_variable_value() {
        let dt = sample_naive_datetime();
        let ev = ElementValue::LocalDateTime(dt);
        let vv: VariableValue = (&ev).into();
        assert_eq!(vv, VariableValue::LocalDateTime(dt));
    }

    #[test]
    fn zoned_datetime_to_variable_value() {
        let dt = sample_fixed_datetime();
        let ev = ElementValue::ZonedDateTime(dt);
        let vv: VariableValue = (&ev).into();
        let expected = VariableValue::ZonedDateTime(VarZonedDateTime::new(dt, None));
        assert_eq!(vv, expected);
    }

    // ── VariableValue → ElementValue (by ref) ──────────────────────────

    #[test]
    fn variable_value_ref_local_datetime_to_element_value() {
        let dt = sample_naive_datetime();
        let vv = VariableValue::LocalDateTime(dt);
        let ev: ElementValue = (&vv).try_into().unwrap();
        assert_eq!(ev, ElementValue::LocalDateTime(dt));
    }

    #[test]
    fn variable_value_ref_zoned_datetime_to_element_value() {
        let dt = sample_fixed_datetime();
        let vv = VariableValue::ZonedDateTime(VarZonedDateTime::new(dt, None));
        let ev: ElementValue = (&vv).try_into().unwrap();
        assert_eq!(ev, ElementValue::ZonedDateTime(dt));
    }

    // ── VariableValue → ElementValue (by value) ────────────────────────

    #[test]
    fn variable_value_owned_local_datetime_to_element_value() {
        let dt = sample_naive_datetime();
        let vv = VariableValue::LocalDateTime(dt);
        let ev: ElementValue = vv.try_into().unwrap();
        assert_eq!(ev, ElementValue::LocalDateTime(dt));
    }

    #[test]
    fn variable_value_owned_zoned_datetime_to_element_value() {
        let dt = sample_fixed_datetime();
        let vv = VariableValue::ZonedDateTime(VarZonedDateTime::new(dt, None));
        let ev: ElementValue = vv.try_into().unwrap();
        assert_eq!(ev, ElementValue::ZonedDateTime(dt));
    }

    // ── ElementValue → serde_json::Value ───────────────────────────────

    #[test]
    fn local_datetime_to_json() {
        let dt = sample_naive_datetime();
        let ev = ElementValue::LocalDateTime(dt);
        let json: serde_json::Value = (&ev).into();
        assert_eq!(json, serde_json::Value::String(dt.to_string()));
    }

    #[test]
    fn zoned_datetime_to_json() {
        let dt = sample_fixed_datetime();
        let ev = ElementValue::ZonedDateTime(dt);
        let json: serde_json::Value = (&ev).into();
        assert_eq!(json, serde_json::Value::String(dt.to_rfc3339()));
    }

    // ── Round-trip: ElementValue → VariableValue → ElementValue ────────

    #[test]
    fn roundtrip_local_datetime() {
        let dt = sample_naive_datetime();
        let original = ElementValue::LocalDateTime(dt);
        let vv: VariableValue = (&original).into();
        let recovered: ElementValue = vv.try_into().unwrap();
        assert_eq!(original, recovered);
    }

    #[test]
    fn roundtrip_zoned_datetime() {
        let dt = sample_fixed_datetime();
        let original = ElementValue::ZonedDateTime(dt);
        let vv: VariableValue = (&original).into();
        let recovered: ElementValue = vv.try_into().unwrap();
        assert_eq!(original, recovered);
    }

    // ── JSON round-trip (note: JSON strings do NOT auto-parse to datetime) ──

    #[test]
    fn json_string_does_not_become_datetime() {
        // A JSON string that looks like a timestamp should remain an
        // ElementValue::String, not magically become LocalDateTime.
        let json = serde_json::Value::String("2024-06-15 10:30:45".to_string());
        let ev: ElementValue = (&json).into();
        assert!(matches!(ev, ElementValue::String(_)));
    }

    // ── Other variants still work ──────────────────────────────────────

    #[test]
    fn null_roundtrip() {
        let ev = ElementValue::Null;
        let vv: VariableValue = (&ev).into();
        assert_eq!(vv, VariableValue::Null);
        let recovered: ElementValue = vv.try_into().unwrap();
        assert_eq!(recovered, ElementValue::Null);
    }

    #[test]
    fn bool_roundtrip() {
        let ev = ElementValue::Bool(true);
        let vv: VariableValue = (&ev).into();
        assert_eq!(vv, VariableValue::Bool(true));
    }

    #[test]
    fn integer_roundtrip() {
        let ev = ElementValue::Integer(42);
        let vv: VariableValue = (&ev).into();
        assert_eq!(vv, VariableValue::Integer(Integer::from(42i64)));
    }

    #[test]
    fn zoned_datetime_utc() {
        let dt = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .unwrap()
            .fixed_offset();
        let ev = ElementValue::ZonedDateTime(dt);
        let vv: VariableValue = (&ev).into();
        let recovered: ElementValue = vv.try_into().unwrap();
        assert_eq!(recovered, ev);
    }
}
