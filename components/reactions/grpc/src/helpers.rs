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

//! Helper utilities for converting data to Protocol Buffer format.
//!
//! These functions handle the conversion from JSON values to protobuf Struct
//! and Value types, which are used in gRPC messages.

use std::collections::BTreeMap;

/// Convert JSON value to protobuf Struct
///
/// Shared helper used by the gRPC reaction runners and the dynamic-plugin
/// descriptor when building `ProtoQueryResultItem.data`.
///
/// # Arguments
/// * `value` - JSON value to convert
///
/// # Returns
/// A protobuf Struct representation of the JSON value
pub fn convert_json_to_proto_struct(value: &serde_json::Value) -> prost_types::Struct {
    use prost_types::Struct;

    match value {
        serde_json::Value::Object(map) => {
            let mut fields = BTreeMap::new();
            for (key, val) in map {
                fields.insert(key.clone(), convert_json_to_proto_value(val));
            }
            Struct { fields }
        }
        _ => {
            // If it's not an object, wrap it in an object with a "value" key
            let mut fields = BTreeMap::new();
            fields.insert("value".to_string(), convert_json_to_proto_value(value));
            Struct { fields }
        }
    }
}

/// Convert JSON value to protobuf Value
///
/// Helper for converting individual JSON values to Protocol Buffer Value type.
/// This is public to support conversion utilities in other gRPC-based reactions.
///
/// # Arguments
/// * `value` - JSON value to convert
///
/// # Returns
/// A protobuf Value representation of the JSON value
pub fn convert_json_to_proto_value(value: &serde_json::Value) -> prost_types::Value {
    use prost_types::{value::Kind, ListValue, Value};

    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(*b),
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                Kind::NumberValue(f)
            } else {
                Kind::StringValue(n.to_string())
            }
        }
        serde_json::Value::String(s) => Kind::StringValue(s.clone()),
        serde_json::Value::Array(arr) => {
            let values: Vec<Value> = arr.iter().map(convert_json_to_proto_value).collect();
            Kind::ListValue(ListValue { values })
        }
        serde_json::Value::Object(_) => Kind::StructValue(convert_json_to_proto_struct(value)),
    };

    Value { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::value::Kind;
    use serde_json::json;

    fn kind(value: &prost_types::Value) -> &Kind {
        value.kind.as_ref().expect("proto value carries a kind")
    }

    #[test]
    fn object_maps_to_struct_fields_one_to_one() {
        let s = convert_json_to_proto_struct(&json!({"a": 1, "b": "two"}));
        assert_eq!(s.fields.len(), 2);
        assert!(matches!(kind(&s.fields["a"]), Kind::NumberValue(n) if *n == 1.0));
        assert!(matches!(kind(&s.fields["b"]), Kind::StringValue(v) if v == "two"));
    }

    #[test]
    fn empty_object_maps_to_empty_struct() {
        let s = convert_json_to_proto_struct(&json!({}));
        assert!(s.fields.is_empty());
    }

    #[test]
    fn scalar_is_wrapped_under_value_key() {
        // Non-object inputs are wrapped as `{"value": <scalar>}` so the result
        // is always a proto Struct.
        for input in [
            json!(42),
            json!("hi"),
            json!(true),
            json!(null),
            json!([1, 2]),
        ] {
            let s = convert_json_to_proto_struct(&input);
            assert_eq!(s.fields.len(), 1, "scalar must wrap into a single field");
            assert!(
                s.fields.contains_key("value"),
                "wrapper key must be `value`"
            );
        }
    }

    #[test]
    fn null_bool_string_number_values_convert() {
        assert!(matches!(
            kind(&convert_json_to_proto_value(&json!(null))),
            Kind::NullValue(0)
        ));
        assert!(matches!(
            kind(&convert_json_to_proto_value(&json!(true))),
            Kind::BoolValue(true)
        ));
        assert!(matches!(
            kind(&convert_json_to_proto_value(&json!("x"))),
            Kind::StringValue(s) if s == "x"
        ));
        assert!(matches!(
            kind(&convert_json_to_proto_value(&json!(-3.5))),
            Kind::NumberValue(n) if *n == -3.5
        ));
    }

    #[test]
    fn integer_numbers_become_f64_number_values() {
        // serde_json integers convert through `as_f64`, so they arrive on the
        // wire as proto NumberValue (f64), not strings.
        assert!(matches!(
            kind(&convert_json_to_proto_value(&json!(7))),
            Kind::NumberValue(n) if *n == 7.0
        ));
        assert!(matches!(
            kind(&convert_json_to_proto_value(&json!(u64::MAX))),
            Kind::NumberValue(_)
        ));
    }

    #[test]
    fn arrays_convert_to_list_values_preserving_order() {
        let v = convert_json_to_proto_value(&json!([1, "a", true]));
        let Kind::ListValue(list) = kind(&v) else {
            panic!("expected a ListValue");
        };
        assert_eq!(list.values.len(), 3);
        assert!(matches!(kind(&list.values[0]), Kind::NumberValue(n) if *n == 1.0));
        assert!(matches!(kind(&list.values[1]), Kind::StringValue(s) if s == "a"));
        assert!(matches!(kind(&list.values[2]), Kind::BoolValue(true)));
    }

    #[test]
    fn nested_objects_and_arrays_convert_recursively() {
        let s = convert_json_to_proto_struct(&json!({
            "outer": {"inner": [ {"k": "v"} ]}
        }));
        let Kind::StructValue(outer) = kind(&s.fields["outer"]) else {
            panic!("expected nested StructValue");
        };
        let Kind::ListValue(list) = kind(&outer.fields["inner"]) else {
            panic!("expected nested ListValue");
        };
        let Kind::StructValue(elem) = kind(&list.values[0]) else {
            panic!("expected struct element inside the list");
        };
        assert!(matches!(kind(&elem.fields["k"]), Kind::StringValue(s) if s == "v"));
    }
}
