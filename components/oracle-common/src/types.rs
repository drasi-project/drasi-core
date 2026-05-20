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

//! Oracle row to Drasi element value conversion.

use anyhow::Result;
use drasi_core::models::{ElementPropertyMap, ElementValue};
use oracle::sql_type::{OracleType, Timestamp};
use oracle::Row;
use ordered_float::OrderedFloat;
use std::sync::Arc;

pub fn extract_column_value(row: &Row, idx: usize) -> Result<ElementValue> {
    let column_info = &row.column_info()[idx];
    let value = match column_info.oracle_type() {
        OracleType::Boolean => row
            .get::<_, bool>(idx)
            .map(ElementValue::Bool)
            .unwrap_or(ElementValue::Null),
        OracleType::Number(_, _) => row
            .get::<_, i64>(idx)
            .map(ElementValue::Integer)
            .or_else(|_| {
                row.get::<_, f64>(idx)
                    .map(|value| ElementValue::Float(OrderedFloat(value)))
            })
            .unwrap_or(ElementValue::Null),
        OracleType::BinaryFloat | OracleType::BinaryDouble => row
            .get::<_, f64>(idx)
            .map(|value| ElementValue::Float(OrderedFloat(value)))
            .unwrap_or(ElementValue::Null),
        OracleType::Varchar2(_)
        | OracleType::NVarchar2(_)
        | OracleType::Char(_)
        | OracleType::NChar(_)
        | OracleType::Long => row
            .get::<_, String>(idx)
            .map(|value| ElementValue::String(Arc::from(value)))
            .unwrap_or(ElementValue::Null),
        OracleType::Date
        | OracleType::Timestamp(_)
        | OracleType::TimestampTZ(_)
        | OracleType::TimestampLTZ(_) => row
            .get::<_, Timestamp>(idx)
            .map(timestamp_to_element_value)
            .unwrap_or(ElementValue::Null),
        OracleType::CLOB | OracleType::NCLOB => row
            .get::<_, String>(idx)
            .map(|value| ElementValue::String(Arc::from(value)))
            .unwrap_or(ElementValue::Null),
        OracleType::BLOB | OracleType::Raw(_) | OracleType::LongRaw => row
            .get::<_, Vec<u8>>(idx)
            .map(|bytes| ElementValue::String(Arc::from(format!("0x{}", hex::encode(bytes)))))
            .unwrap_or(ElementValue::Null),
        _ => row
            .get::<_, String>(idx)
            .map(|value| ElementValue::String(Arc::from(value)))
            .unwrap_or(ElementValue::Null),
    };

    Ok(value)
}

pub fn extract_row_properties(row: &Row) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();
    for (idx, column_info) in row.column_info().iter().enumerate() {
        properties.insert(
            &column_info.name().to_lowercase(),
            extract_column_value(row, idx)?,
        );
    }
    Ok(properties)
}

pub fn value_to_string(value: &ElementValue) -> String {
    match value {
        ElementValue::Null => "null".to_string(),
        ElementValue::Bool(value) => value.to_string(),
        ElementValue::Float(value) => value.to_string(),
        ElementValue::Integer(value) => value.to_string(),
        ElementValue::String(value) => value.to_string(),
        ElementValue::List(value) => format!("{value:?}"),
        ElementValue::Object(value) => format!("{value:?}"),
    }
}

pub fn sql_literal_to_element_value(literal: &str) -> ElementValue {
    let trimmed = literal.trim();

    if trimmed.eq_ignore_ascii_case("null") {
        return ElementValue::Null;
    }

    if trimmed.eq_ignore_ascii_case("true") {
        return ElementValue::Bool(true);
    }

    if trimmed.eq_ignore_ascii_case("false") {
        return ElementValue::Bool(false);
    }

    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        return ElementValue::String(Arc::from(trimmed[1..trimmed.len() - 1].replace("''", "'")));
    }

    if let Ok(value) = trimmed.parse::<i64>() {
        return ElementValue::Integer(value);
    }

    if let Ok(value) = trimmed.parse::<f64>() {
        return ElementValue::Float(OrderedFloat(value));
    }

    ElementValue::String(Arc::from(trimmed.to_string()))
}

fn timestamp_to_element_value(timestamp: Timestamp) -> ElementValue {
    let rendered = if timestamp.tz_hour_offset() != 0 || timestamp.tz_minute_offset() != 0 {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}{:+03}:{:02}",
            timestamp.year(),
            timestamp.month(),
            timestamp.day(),
            timestamp.hour(),
            timestamp.minute(),
            timestamp.second(),
            timestamp.nanosecond(),
            timestamp.tz_hour_offset(),
            timestamp.tz_minute_offset().unsigned_abs()
        )
    } else {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
            timestamp.year(),
            timestamp.month(),
            timestamp.day(),
            timestamp.hour(),
            timestamp.minute(),
            timestamp.second(),
            timestamp.nanosecond()
        )
    };

    ElementValue::String(Arc::from(rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_to_string() {
        assert_eq!(value_to_string(&ElementValue::Integer(42)), "42");
        assert_eq!(value_to_string(&ElementValue::Bool(true)), "true");
        assert_eq!(value_to_string(&ElementValue::Null), "null");
    }

    #[test]
    fn test_sql_literal_to_element_value() {
        assert_eq!(sql_literal_to_element_value("null"), ElementValue::Null);
        assert_eq!(
            sql_literal_to_element_value("42"),
            ElementValue::Integer(42)
        );
        assert_eq!(
            sql_literal_to_element_value("'it''s fine'"),
            ElementValue::String(Arc::from("it's fine"))
        );
        assert_eq!(
            sql_literal_to_element_value("3.5"),
            ElementValue::Float(OrderedFloat(3.5))
        );
    }
}
