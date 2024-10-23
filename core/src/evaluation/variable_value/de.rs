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

use serde::de::{Deserialize, Deserializer};

use super::VariableValue;
use serde_json::Value;

impl<'de> Deserialize<'de> for VariableValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize JSON into serde_json::Value first
        let value: Value = Deserialize::deserialize(deserializer)?;

        match value {
            Value::Null => Ok(VariableValue::Null),
            Value::Bool(b) => Ok(VariableValue::from(Value::Bool(b))),
            Value::Number(n) => Ok(VariableValue::from(Value::Number(n))),
            Value::String(s) => Ok(VariableValue::from(Value::String(s))),
            Value::Array(a) => Ok(VariableValue::from(Value::Array(a))),
            Value::Object(o) => Ok(VariableValue::from(Value::Object(o))),
        }
    }
}
