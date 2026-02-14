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

use anyhow::Result;
use drasi_core::models::{ElementPropertyMap, ElementValue};
use neo4rs::{BoltMap, BoltType};
use ordered_float::OrderedFloat;
use std::sync::Arc;

pub fn bolt_type_to_element_value(value: &BoltType) -> Result<ElementValue> {
    Ok(match value {
        BoltType::Null(_) => ElementValue::Null,
        BoltType::Boolean(v) => ElementValue::Bool(v.value),
        BoltType::Integer(v) => ElementValue::Integer(v.value),
        BoltType::Float(v) => ElementValue::Float(OrderedFloat(v.value)),
        BoltType::String(v) => ElementValue::String(Arc::from(v.value.as_str())),
        BoltType::List(v) => ElementValue::List(
            v.value
                .iter()
                .map(bolt_type_to_element_value)
                .collect::<Result<Vec<_>>>()?,
        ),
        BoltType::Map(v) => ElementValue::Object(bolt_map_to_element_properties(v)?),
        // Temporal / spatial / graph-specific values are represented as strings for portability.
        other => ElementValue::String(Arc::from(other.to_string())),
    })
}

pub fn bolt_map_to_element_properties(map: &BoltMap) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();
    for (key, value) in &map.value {
        properties.insert(&key.value, bolt_type_to_element_value(value)?);
    }
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_property() {
        let value = BoltType::String("hello".into());
        let converted = bolt_type_to_element_value(&value).unwrap();
        assert_eq!(converted, ElementValue::String(Arc::from("hello")));
    }

    #[test]
    fn test_integer_property() {
        let value = BoltType::Integer(42_i64.into());
        let converted = bolt_type_to_element_value(&value).unwrap();
        assert_eq!(converted, ElementValue::Integer(42));
    }

    #[test]
    fn test_float_property() {
        let value = BoltType::Float(neo4rs::BoltFloat { value: 2.5_f64 });
        let converted = bolt_type_to_element_value(&value).unwrap();
        assert_eq!(converted, ElementValue::Float(OrderedFloat(2.5)));
    }

    #[test]
    fn test_bool_property() {
        let value = BoltType::Boolean(neo4rs::BoltBoolean { value: true });
        let converted = bolt_type_to_element_value(&value).unwrap();
        assert_eq!(converted, ElementValue::Bool(true));
    }

    #[test]
    fn test_null_property() {
        let value = BoltType::Null(neo4rs::BoltNull);
        let converted = bolt_type_to_element_value(&value).unwrap();
        assert_eq!(converted, ElementValue::Null);
    }
}
