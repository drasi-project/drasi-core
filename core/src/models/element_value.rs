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

use crate::evaluation::variable_value::{float::Float, integer::Integer, VariableValue};

use std::sync::Arc;

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
                // Handle infinity and NaN - JSON doesn't support them, so convert to null
                serde_json::Number::from_f64(f.into_inner())
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
            ElementValue::Integer(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
            ElementValue::String(s) => serde_json::Value::String(s.to_string()),
            ElementValue::List(l) => serde_json::Value::Array(l.iter().map(|x| x.into()).collect()),
            ElementValue::Object(o) => serde_json::Value::Object(o.into()),
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
