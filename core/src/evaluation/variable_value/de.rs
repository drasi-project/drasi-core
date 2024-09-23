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
