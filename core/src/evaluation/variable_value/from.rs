use super::VariableValue;
extern crate alloc;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use alloc::borrow::Cow;
use chrono::{NaiveDate, NaiveTime};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

macro_rules! from_integer {
    ($($ty:ident)*) => {
        $(
            impl From<$ty> for VariableValue {
                fn from(n: $ty) -> Self {
                    VariableValue::Integer(n.into())
                }
            }
        )*
    };
}

from_integer! {
    i8 i16 i32 i64 isize
    u8 u16 u32 u64 usize
}

#[cfg(feature = "arbitrary_precision")]
from_integer! {
    i128 u128
}

impl From<f32> for VariableValue {
    fn from(n: f32) -> Self {
        Float::from_f32(n).map_or(VariableValue::Null, VariableValue::Float)
    }
}

impl From<f64> for VariableValue {
    fn from(n: f64) -> Self {
        Float::from_f64(n).map_or(VariableValue::Null, VariableValue::Float)
    }
}

impl From<bool> for VariableValue {
    fn from(b: bool) -> Self {
        VariableValue::Bool(b)
    }
}

impl From<String> for VariableValue {
    fn from(s: String) -> Self {
        if let Ok(date) = NaiveDate::parse_from_str(&s, "%F") {
            // %Y-%m-%d
            VariableValue::Date(date)
        } else {
            VariableValue::String(s)
        }
    }
}

impl<'a> From<&'a str> for VariableValue {
    fn from(s: &'a str) -> Self {
        if let Ok(date) = NaiveDate::parse_from_str(s, "%F") {
            // %Y-%m-%d
            VariableValue::Date(date)
        } else {
            VariableValue::String(s.to_string())
        }
    }
}

impl<'a> From<Cow<'a, str>> for VariableValue {
    fn from(s: Cow<'a, str>) -> Self {
        VariableValue::String(s.into_owned())
    }
}

impl From<Integer> for VariableValue {
    fn from(n: Integer) -> Self {
        VariableValue::Integer(n)
    }
}

impl From<Float> for VariableValue {
    fn from(n: Float) -> Self {
        VariableValue::Float(n)
    }
}

impl From<BTreeMap<String, VariableValue>> for VariableValue {
    fn from(m: BTreeMap<String, VariableValue>) -> Self {
        VariableValue::Object(m)
    }
}

impl From<Map<String, Value>> for VariableValue {
    fn from(m: Map<String, Value>) -> Self {
        VariableValue::Object(m.into_iter().map(|(k, v)| (k, v.into())).collect())
    }
}

impl<T: Into<VariableValue>> From<Vec<T>> for VariableValue {
    fn from(v: Vec<T>) -> Self {
        VariableValue::List(v.into_iter().map(|e| e.into()).collect())
    }
}

impl<'a, T: Clone + Into<VariableValue>> From<&'a [T]> for VariableValue {
    // Convert a slice to a value
    fn from(v: &'a [T]) -> Self {
        VariableValue::List(v.iter().cloned().map(Into::into).collect())
    }
}

impl<T: Into<VariableValue>> FromIterator<T> for VariableValue {
    // Convert an iterable type to a value
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        VariableValue::List(iter.into_iter().map(Into::into).collect())
    }
}

impl<K: Into<String>, V: Into<VariableValue>> FromIterator<(K, V)> for VariableValue {
    // Convert an iterable type to a value
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        VariableValue::Object(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl From<()> for VariableValue {
    // Convert `()` to a null value
    fn from((): ()) -> Self {
        VariableValue::Null
    }
}

impl<T> From<Option<T>> for VariableValue
where
    T: Into<VariableValue>,
{
    fn from(o: Option<T>) -> Self {
        match o {
            Some(v) => Into::into(v),
            None => VariableValue::Null,
        }
    }
}

impl From<ZonedTime> for VariableValue {
    fn from(date_time: ZonedTime) -> Self {
        VariableValue::ZonedTime(date_time)
    }
}

impl From<ZonedDateTime> for VariableValue {
    fn from(date_time: ZonedDateTime) -> Self {
        VariableValue::ZonedDateTime(date_time)
    }
}

impl From<NaiveTime> for VariableValue {
    fn from(time: NaiveTime) -> Self {
        VariableValue::LocalTime(time)
    }
}

impl From<Value> for VariableValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => VariableValue::Null,
            Value::Bool(b) => VariableValue::Bool(b),
            Value::Number(num) => {
                if let Some(n) = num.as_i64() {
                    VariableValue::Integer(n.into())
                } else {
                    VariableValue::Float(Float::from_f64(num.as_f64().unwrap()).unwrap())
                }
            }
            Value::String(s) => VariableValue::String(s),
            Value::Array(arr) => {
                let variable_values: Vec<VariableValue> = arr
                    .into_iter()
                    .map(VariableValue::from)
                    .collect();
                VariableValue::List(variable_values)
            }
            Value::Object(obj) => {
                let variable_values: BTreeMap<String, VariableValue> = obj
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), VariableValue::from(v)))
                    .collect();
                VariableValue::Object(variable_values)
            }
        }
    }
}
