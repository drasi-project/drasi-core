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
            VariableValue::Integer(v) => serializer.serialize_i64(v.as_i64().unwrap()),
            VariableValue::Float(v) => serializer.serialize_f64(v.as_f64().unwrap()),
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
            VariableValue::ZonedTime(_v) => todo!(),
            VariableValue::LocalDateTime(v) => v.serialize(serializer),
            VariableValue::ZonedDateTime(v) => v.serialize(serializer),
            VariableValue::Duration(_v) => todo!(),
            VariableValue::Expression(_v) => todo!(),
            VariableValue::ListRange(_v) => todo!(),
            VariableValue::Element(_) => todo!(),
            VariableValue::ElementMetadata(_m) => todo!(),
            VariableValue::ElementReference(_) => todo!(),
            VariableValue::Awaiting => todo!(),
        }
    }
}
