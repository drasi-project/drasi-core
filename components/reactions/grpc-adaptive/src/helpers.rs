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

//! Helper utilities for converting data to Protocol Buffer format.
//!
//! These functions handle the conversion from JSON values to protobuf Struct
//! and Value types, which are used in gRPC messages.

use std::collections::BTreeMap;

/// Convert JSON value to protobuf Struct
///
/// This is a public utility that can be used by gRPC-based reactions
/// to convert JSON data to Protocol Buffer format.
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
