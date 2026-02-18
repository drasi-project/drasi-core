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

use super::VariableValue;
use serde::ser::{Serialize, Serializer};

impl Serialize for VariableValue {
    #[inline]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            VariableValue::Null => serializer.serialize_unit(),
            VariableValue::Bool(v) => serializer.serialize_bool(*v),
            VariableValue::Integer(v) => serializer.serialize_i64(match v.as_i64() {
                Some(v) => v,
                None => return Err(serde::ser::Error::custom("Integer overflow")),
            }),
            VariableValue::Float(v) => serializer.serialize_f64(match v.as_f64() {
                Some(v) => v,
                None => return Err(serde::ser::Error::custom("Float overflow")),
            }),
            VariableValue::String(s) => serializer.serialize_str(s),
            VariableValue::List(v) => v.serialize(serializer),
            VariableValue::Object(m) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(m.len()))?;
                for (k, v) in m {
                    let _ = map.serialize_entry(k, v);
                }
                map.end()
            }
            VariableValue::Date(v) => v.serialize(serializer),
            VariableValue::LocalTime(v) => v.serialize(serializer),
            VariableValue::ZonedTime(v) => serializer.serialize_str(&v.to_string()),
            VariableValue::LocalDateTime(v) => v.serialize(serializer),
            VariableValue::ZonedDateTime(v) => v.serialize(serializer),
            VariableValue::Duration(v) => serializer.serialize_str(&v.to_string()),
            VariableValue::Expression(v) => serializer.serialize_str(&format!("{v:?}")),
            VariableValue::ListRange(v) => serializer.serialize_str(&v.to_string()),
            VariableValue::Element(e) => {
                let json_value: serde_json::Value = e.as_ref().into();
                json_value.serialize(serializer)
            }
            VariableValue::ElementMetadata(m) => serializer.serialize_str(&m.to_string()),
            VariableValue::ElementReference(r) => serializer.serialize_str(&r.to_string()),
            VariableValue::Awaiting => serializer.serialize_str("Awaiting"),
        }
    }
}
