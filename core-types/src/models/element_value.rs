// Copyright 2025 The Drasi Authors.
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

#![allow(clippy::unwrap_used)]
use super::ConversionError;

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{Index, IndexMut},
};

use crate::data_value::{DataValue, DataValueError};
use crate::evaluation::variable_value::{
    float::Float, integer::Integer, zoned_datetime::ZonedDateTime as VarZonedDateTime,
    VariableValue,
};

use std::sync::Arc;

use chrono::{DateTime, FixedOffset, NaiveDateTime};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Hash, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum ElementValue {
    #[default]
    Null,
    Bool(bool),
    Float(#[serde(with = "float_image")] OrderedFloat<f64>),
    Integer(i64),
    String(Arc<str>),
    List(Vec<ElementValue>),
    Object(ElementPropertyMap),
    LocalDateTime(NaiveDateTime),
    // ElementValue stores zoned datetimes as fixed offsets only; optional
    // IANA timezone names from VariableValue are not represented here.
    ZonedDateTime(DateTime<FixedOffset>),
    /// Rich values retain their typed representation without enlarging primitive properties.
    Typed(Box<TypedElementValue>),
}

impl ElementValue {
    /// Decode the typed JSON projection, rejecting malformed typed-value envelopes.
    ///
    /// Plain JSON construction remains available through `From`; this method
    /// additionally restores typed values produced by the JSON projection.
    pub fn try_from_data_json(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        use serde::de::Error;

        match value {
            serde_json::Value::Object(object)
                if object
                    .get(DRASI_ENVELOPE_MARKER_TAG)
                    .and_then(|v| v.as_str())
                    == Some(DRASI_DATA_VALUE_MARKER) =>
            {
                let payload = object
                    .get("value")
                    .ok_or_else(|| serde_json::Error::custom("typed value has no payload"))?;
                let value = serde_json::from_value::<DataValue>(payload.clone())?
                    .decode()
                    .map_err(serde_json::Error::custom)?;
                (&value).try_into().map_err(serde_json::Error::custom)
            }
            serde_json::Value::Object(object)
                if object
                    .get(DRASI_ENVELOPE_MARKER_TAG)
                    .and_then(|v| v.as_str())
                    == Some(DRASI_ENVELOPE_MARKER_VALUE) =>
            {
                Ok(value.into())
            }
            serde_json::Value::Object(object) => {
                let mut properties = ElementPropertyMap::new();
                for (name, value) in object {
                    properties.insert(name, Self::try_from_data_json(value)?);
                }
                Ok(Self::Object(properties))
            }
            serde_json::Value::Array(values) => Ok(Self::List(
                values
                    .iter()
                    .map(Self::try_from_data_json)
                    .collect::<Result<_, _>>()?,
            )),
            _ => Ok(value.into()),
        }
    }
}

/// A validated value outside the compact primitive property representation.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TypedElementValue(DataValue);

impl TypedElementValue {
    pub fn as_data_value(&self) -> &DataValue {
        &self.0
    }

    fn from_variable(value: &VariableValue) -> Result<Self, DataValueError> {
        Ok(Self(DataValue::encode(value)?))
    }

    fn to_variable(&self) -> VariableValue {
        self.0
            .clone()
            .decode()
            .expect("TypedElementValue is validated at construction and deserialization")
    }
}

impl TryFrom<DataValue> for TypedElementValue {
    type Error = DataValueError;

    fn try_from(value: DataValue) -> Result<Self, Self::Error> {
        value.clone().decode()?;
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for TypedElementValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        DataValue::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

mod float_image {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &OrderedFloat<f64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if !serializer.is_human_readable() {
            return value.serialize(serializer);
        }
        #[derive(Serialize)]
        struct Bits {
            bits: String,
        }
        Bits {
            bits: format!("{:016x}", value.0.to_bits()),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OrderedFloat<f64>, D::Error> {
        if !deserializer.is_human_readable() {
            return OrderedFloat::deserialize(deserializer);
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Image {
            Number(f64),
            Bits { bits: String },
        }
        match Image::deserialize(deserializer)? {
            Image::Number(value) => Ok(OrderedFloat(value)),
            Image::Bits { bits } => {
                if bits.len() != 16 {
                    return Err(serde::de::Error::custom("expected 16 float bit digits"));
                }
                let bits = u64::from_str_radix(&bits, 16).map_err(serde::de::Error::custom)?;
                Ok(OrderedFloat(f64::from_bits(bits)))
            }
        }
    }
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
                // ElementValue cannot carry IANA timezone names, so we emit None.
                VariableValue::ZonedDateTime(VarZonedDateTime::new(*dt, None))
            }
            ElementValue::Typed(value) => value.to_variable(),
        }
    }
}

impl TryInto<ElementValue> for &VariableValue {
    type Error = ConversionError;

    fn try_into(self) -> Result<ElementValue, ConversionError> {
        match self {
            VariableValue::Null => Ok(ElementValue::Null),
            VariableValue::Bool(b) => Ok(ElementValue::Bool(*b)),
            VariableValue::Float(f) => Ok(ElementValue::Float(OrderedFloat(f64::from_bits(
                f.to_bits(),
            )))),
            VariableValue::Integer(i) if i.as_i64().is_some() => Ok(ElementValue::Integer(
                i.as_i64().expect("checked signed integer"),
            )),
            VariableValue::String(s) => Ok(ElementValue::String(Arc::from(s.as_str()))),
            VariableValue::List(l) => Ok(ElementValue::List(
                l.iter().map(|x| x.try_into()).collect::<Result<_, _>>()?,
            )),
            VariableValue::Object(o) => Ok(ElementValue::Object(o.into())),
            VariableValue::LocalDateTime(dt) => Ok(ElementValue::LocalDateTime(*dt)),
            VariableValue::ZonedDateTime(zdt) if zdt.timezone_name().is_none() => {
                Ok(ElementValue::ZonedDateTime(*zdt.datetime()))
            }
            value => TypedElementValue::from_variable(value)
                .map(|value| ElementValue::Typed(Box::new(value)))
                .map_err(|_| ConversionError {}),
        }
    }
}

impl TryInto<ElementValue> for VariableValue {
    type Error = ConversionError;

    fn try_into(self) -> Result<ElementValue, ConversionError> {
        (&self).try_into()
    }
}

/// Tag used inside JSON envelopes to preserve `ElementValue` datetime type
/// information across serialization round-trips.
/// This key is intentionally specific to minimize collisions with user data.
/// This envelope is for internal round-trips, not an external API shape.
const DRASI_TYPE_TAG: &str = "__drasi_v1_type__";
const DRASI_ENVELOPE_MARKER_TAG: &str = "__drasi_v1_envelope__";
const DRASI_ENVELOPE_MARKER_VALUE: &str = "drasi.element_value.datetime";
const DRASI_DATA_VALUE_MARKER: &str = "drasi.data-value";

fn typed_json(value: &DataValue) -> serde_json::Value {
    serde_json::json!({
        DRASI_ENVELOPE_MARKER_TAG: DRASI_DATA_VALUE_MARKER,
        "value": value,
    })
}

impl From<&ElementValue> for serde_json::Value {
    fn from(val: &ElementValue) -> Self {
        match val {
            ElementValue::Null => serde_json::Value::Null,
            ElementValue::Bool(b) => serde_json::Value::Bool(*b),
            ElementValue::Float(f) => serde_json::Number::from_f64(f.into_inner())
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| typed_json(&DataValue::Float(f.0.to_bits()))),
            ElementValue::Integer(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
            ElementValue::String(s) => serde_json::Value::String(s.to_string()),
            ElementValue::List(l) => serde_json::Value::Array(l.iter().map(|x| x.into()).collect()),
            ElementValue::Object(o) => serde_json::Value::Object(o.into()),
            ElementValue::LocalDateTime(dt) => {
                serde_json::json!({
                    DRASI_ENVELOPE_MARKER_TAG: DRASI_ENVELOPE_MARKER_VALUE,
                    DRASI_TYPE_TAG: "drasi.LocalDateTime",
                    "value": dt.to_string()
                })
            }
            ElementValue::ZonedDateTime(dt) => {
                serde_json::json!({
                    DRASI_ENVELOPE_MARKER_TAG: DRASI_ENVELOPE_MARKER_VALUE,
                    DRASI_TYPE_TAG: "drasi.ZonedDateTime",
                    "value": dt.to_rfc3339()
                })
            }
            ElementValue::Typed(value) => typed_json(value.as_data_value()),
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
                } else if let Some(i) = n.as_u64() {
                    ElementValue::Typed(Box::new(TypedElementValue(DataValue::Unsigned(i))))
                } else if let Some(f) = n.as_f64() {
                    ElementValue::Float(OrderedFloat(f))
                } else {
                    ElementValue::Null
                }
            }
            serde_json::Value::String(s) => ElementValue::String(Arc::from(s.as_str())),
            serde_json::Value::Array(a) => ElementValue::List(a.iter().map(|x| x.into()).collect()),
            serde_json::Value::Object(o) => {
                // Check for tagged datetime envelope
                let has_internal_marker = o.get(DRASI_ENVELOPE_MARKER_TAG).and_then(|v| v.as_str())
                    == Some(DRASI_ENVELOPE_MARKER_VALUE);
                if has_internal_marker {
                    if let Some(type_tag) = o.get(DRASI_TYPE_TAG).and_then(|v| v.as_str()) {
                        if let Some(val_str) = o.get("value").and_then(|v| v.as_str()) {
                            match type_tag {
                                "drasi.LocalDateTime" => {
                                    if let Ok(dt) = NaiveDateTime::parse_from_str(
                                        val_str,
                                        "%Y-%m-%d %H:%M:%S%.f",
                                    ) {
                                        return ElementValue::LocalDateTime(dt);
                                    }
                                }
                                "drasi.ZonedDateTime" => {
                                    if let Ok(dt) = DateTime::parse_from_rfc3339(val_str) {
                                        return ElementValue::ZonedDateTime(dt);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                ElementValue::Object(o.into())
            }
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

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
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

    pub fn contains_key(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    pub fn merge(&mut self, other: &ElementPropertyMap) {
        for (key, value) in other.values.iter() {
            self.values
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }

    /// Adds properties missing from this map, except names in `removed`.
    pub fn merge_except(&mut self, other: &ElementPropertyMap, removed: &BTreeSet<String>) {
        for (key, value) in other.values.iter() {
            if !removed.contains(&**key) {
                self.values
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    }

    pub fn map_iter<'a, T>(
        &'a self,
        f: impl Fn(&'a Arc<str>, &'a ElementValue) -> T + 'a,
    ) -> impl Iterator<Item = T> + 'a {
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

    #[test]
    fn property_mapping_can_borrow_values_and_local_context() {
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", ElementValue::List(vec![ElementValue::Integer(7)]));
        let context = String::from("local");
        let (scope, key, value) = properties
            .map_iter(|key, value| (context.as_str(), key.as_ref(), value))
            .next()
            .unwrap();
        assert_eq!((scope, key), ("local", "value"));
        assert!(std::ptr::eq(value, properties.get("value").unwrap()));
    }

    fn assert_rich_roundtrip(value: VariableValue) {
        let expected = rmp_serde::to_vec_named(&DataValue::encode(&value).unwrap()).unwrap();
        let property: ElementValue = (&value).try_into().unwrap();
        let assert_value = |property: &ElementValue| {
            let value: VariableValue = property.into();
            let actual = rmp_serde::to_vec_named(&DataValue::encode(&value).unwrap()).unwrap();
            assert_eq!(actual, expected);
        };
        assert_value(&property);
        let binary = rmp_serde::to_vec_named(&property).unwrap();
        assert_value(&rmp_serde::from_slice::<ElementValue>(&binary).unwrap());
        let serialized = serde_json::to_vec(&property).unwrap();
        assert_value(&serde_json::from_slice::<ElementValue>(&serialized).unwrap());
        let json: serde_json::Value = (&property).into();
        assert_value(&ElementValue::try_from_data_json(&json).unwrap());
    }

    #[test]
    fn rich_query_values_remain_typed_properties() {
        use crate::evaluation::variable_value::{
            duration::Duration, zoned_time::ZonedTime, ListRange, RangeBound,
        };

        let date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let time = date.and_hms_opt(3, 4, 5).unwrap().time();
        let offset = FixedOffset::east_opt(3600).unwrap();
        for value in [
            VariableValue::Integer(Integer::from(u64::MAX)),
            VariableValue::Date(date),
            VariableValue::LocalTime(time),
            VariableValue::ZonedTime(ZonedTime::new(time, offset)),
            VariableValue::ZonedDateTime(VarZonedDateTime::new(
                sample_fixed_datetime(),
                Some("Etc/GMT-1".to_string()),
            )),
            VariableValue::Duration(Duration::new(chrono::Duration::seconds(5), 2, 3)),
            VariableValue::ListRange(ListRange {
                start: RangeBound::Index(2),
                end: RangeBound::Unbounded,
            }),
            VariableValue::ElementReference(super::super::ElementReference::new("source", "row")),
            VariableValue::Expression(drasi_query_ast::ast::Expression::ListExpression(
                drasi_query_ast::ast::ListExpression { elements: vec![] },
            )),
            VariableValue::Awaiting,
        ] {
            assert_rich_roundtrip(value);
        }
    }

    #[test]
    fn nonfinite_and_nested_properties_roundtrip_without_defaulting() {
        for bits in [
            (-0.0_f64).to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            0x7ff8_0000_0000_0042,
        ] {
            assert_rich_roundtrip(VariableValue::Float(Float::from(f64::from_bits(bits))));
        }
        assert_rich_roundtrip(VariableValue::Object(BTreeMap::from([(
            "nested".to_string(),
            VariableValue::List(vec![
                VariableValue::Date(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()),
                VariableValue::Integer(Integer::from(u64::MAX)),
                VariableValue::Float(Float::from(f64::INFINITY)),
            ]),
        )])));
        let mut properties = ElementPropertyMap::new();
        properties.insert(
            "nonfinite",
            ElementValue::Float(OrderedFloat(f64::from_bits(0x7ff8_0000_0000_0042))),
        );
        assert_rich_roundtrip(VariableValue::Element(Arc::new(
            super::super::Element::Node {
                metadata: super::super::ElementMetadata {
                    reference: super::super::ElementReference::new("source", "embedded"),
                    labels: Arc::from([Arc::from("Nested")]),
                    effective_from: 1,
                },
                properties,
            },
        )));
    }

    #[test]
    fn typed_property_deserialization_rejects_invalid_values() {
        let invalid = serde_json::json!({"ZonedTime": ["12:00:00", 999999]});
        assert!(serde_json::from_value::<TypedElementValue>(invalid.clone()).is_err());
        assert!(ElementValue::try_from_data_json(&serde_json::json!({
            "__drasi_v1_envelope__": "drasi.data-value",
            "value": invalid,
        }))
        .is_err());
        assert!(ElementValue::try_from_data_json(&serde_json::json!({
            "__drasi_v1_envelope__": "drasi.data-value",
        }))
        .is_err());
    }

    #[test]
    fn json_unsigned_integer_is_not_rounded_through_float() {
        let property = ElementValue::from(&serde_json::json!(u64::MAX));
        let value: VariableValue = (&property).into();
        assert_eq!(value, VariableValue::Integer(Integer::from(u64::MAX)));
    }

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
        assert_eq!(
            json,
            serde_json::json!({
                "__drasi_v1_envelope__": "drasi.element_value.datetime",
                "__drasi_v1_type__": "drasi.LocalDateTime",
                "value": dt.to_string()
            })
        );
    }

    #[test]
    fn zoned_datetime_to_json() {
        let dt = sample_fixed_datetime();
        let ev = ElementValue::ZonedDateTime(dt);
        let json: serde_json::Value = (&ev).into();
        assert_eq!(
            json,
            serde_json::json!({
                "__drasi_v1_envelope__": "drasi.element_value.datetime",
                "__drasi_v1_type__": "drasi.ZonedDateTime",
                "value": dt.to_rfc3339()
            })
        );
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

    // ── JSON round-trip via tagged envelope ─────────────────────────────

    #[test]
    fn json_roundtrip_local_datetime() {
        let dt = sample_naive_datetime();
        let original = ElementValue::LocalDateTime(dt);
        let json: serde_json::Value = (&original).into();
        let recovered: ElementValue = (&json).into();
        assert_eq!(original, recovered);
    }

    #[test]
    fn json_roundtrip_zoned_datetime() {
        let dt = sample_fixed_datetime();
        let original = ElementValue::ZonedDateTime(dt);
        let json: serde_json::Value = (&original).into();
        let recovered: ElementValue = (&json).into();
        assert_eq!(original, recovered);
    }

    #[test]
    fn json_roundtrip_zoned_datetime_utc() {
        let dt = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .unwrap()
            .fixed_offset();
        let original = ElementValue::ZonedDateTime(dt);
        let json: serde_json::Value = (&original).into();
        let recovered: ElementValue = (&json).into();
        assert_eq!(original, recovered);
    }

    #[test]
    fn tagged_object_with_unknown_type_stays_object() {
        // An object with the internal tag set to an unknown type should
        // be deserialized as a regular Object, not a datetime.
        let json = serde_json::json!({
            "__drasi_v1_envelope__": "drasi.element_value.datetime",
            "__drasi_v1_type__": "UnknownType",
            "value": "some-value"
        });
        let ev: ElementValue = (&json).into();
        assert!(matches!(ev, ElementValue::Object(_)));
    }

    #[test]
    fn tagged_object_with_invalid_datetime_stays_object() {
        // A tagged envelope with an unparseable value should fall back
        // to Object rather than panic.
        let json = serde_json::json!({
            "__drasi_v1_envelope__": "drasi.element_value.datetime",
            "__drasi_v1_type__": "drasi.ZonedDateTime",
            "value": "not-a-datetime"
        });
        let ev: ElementValue = (&json).into();
        assert!(matches!(ev, ElementValue::Object(_)));
    }

    #[test]
    fn object_with_common_type_key_does_not_get_interpreted_as_datetime() {
        // User objects using a common `_type` key must not collide with internal tags.
        let json = serde_json::json!({
            "_type": "drasi.LocalDateTime",
            "value": "2024-06-15 10:30:45",
            "timezone_name": "UTC"
        });
        let ev: ElementValue = (&json).into();
        assert!(matches!(ev, ElementValue::Object(_)));
    }

    #[test]
    fn object_with_internal_type_but_no_marker_stays_object() {
        // Objects that happen to contain the internal type key are not interpreted
        // as datetime envelopes unless the dedicated marker is also present.
        let json = serde_json::json!({
            "__drasi_v1_type__": "drasi.ZonedDateTime",
            "value": "1970-01-01T00:00:00+00:00"
        });
        let ev: ElementValue = (&json).into();
        assert!(matches!(ev, ElementValue::Object(_)));
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
