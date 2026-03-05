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

//! Type conversion from MS SQL to Drasi ElementValue

use anyhow::Result;
use drasi_core::models::ElementValue;
use std::sync::Arc;
use tiberius::Row;

/// Extract a column value from a row by index and convert to ElementValue
///
/// Uses column type metadata to select the correct conversion, consistent with
/// the bootstrapper's `convert_column_value` approach.
pub fn extract_column_value(row: &Row, col_idx: usize) -> Result<ElementValue> {
    use tiberius::ColumnType;

    let column = &row.columns()[col_idx];

    match column.column_type() {
        ColumnType::Bit | ColumnType::Bitn => {
            if let Ok(Some(val)) = row.try_get::<bool, _>(col_idx) {
                Ok(ElementValue::Bool(val))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => {
            if let Ok(Some(val)) = row.try_get::<i32, _>(col_idx) {
                Ok(ElementValue::Integer(val as i64))
            } else if let Ok(Some(val)) = row.try_get::<i64, _>(col_idx) {
                Ok(ElementValue::Integer(val))
            } else if let Ok(Some(val)) = row.try_get::<i16, _>(col_idx) {
                Ok(ElementValue::Integer(val as i64))
            } else if let Ok(Some(val)) = row.try_get::<u8, _>(col_idx) {
                Ok(ElementValue::Integer(val as i64))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => {
            if let Ok(Some(val)) = row.try_get::<f32, _>(col_idx) {
                Ok(ElementValue::Float(ordered_float::OrderedFloat(val as f64)))
            } else if let Ok(Some(val)) = row.try_get::<f64, _>(col_idx) {
                Ok(ElementValue::Float(ordered_float::OrderedFloat(val)))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::Numericn | ColumnType::Decimaln => {
            if let Ok(Some(d)) = row.try_get::<rust_decimal::Decimal, _>(col_idx) {
                let f = d.to_string().parse::<f64>().unwrap_or(0.0);
                Ok(ElementValue::Float(ordered_float::OrderedFloat(f)))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText => {
            if let Ok(Some(val)) = row.try_get::<&str, _>(col_idx) {
                Ok(ElementValue::String(Arc::from(val)))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::Guid => {
            if let Ok(Some(val)) = row.try_get::<uuid::Uuid, _>(col_idx) {
                Ok(ElementValue::String(Arc::from(val.to_string())))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::Datetime
        | ColumnType::Datetime2
        | ColumnType::Datetime4
        | ColumnType::Datetimen
        | ColumnType::Daten => {
            if let Ok(Some(val)) = row.try_get::<chrono::NaiveDateTime, _>(col_idx) {
                Ok(ElementValue::String(Arc::from(
                    val.format("%Y-%m-%dT%H:%M:%S%.3f").to_string(),
                )))
            } else if let Ok(Some(val)) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(col_idx) {
                Ok(ElementValue::String(Arc::from(val.to_rfc3339())))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => {
            if let Ok(Some(bytes)) = row.try_get::<&[u8], _>(col_idx) {
                Ok(ElementValue::String(Arc::from(format!(
                    "0x{}",
                    hex::encode(bytes)
                ))))
            } else {
                Ok(ElementValue::Null)
            }
        }
        _ => {
            // For unsupported types, try to convert to string
            if let Ok(Some(val)) = row.try_get::<&str, _>(col_idx) {
                Ok(ElementValue::String(Arc::from(val)))
            } else {
                log::warn!(
                    "Unsupported column type {:?} for column {}, treating as NULL",
                    column.column_type(),
                    column.name()
                );
                Ok(ElementValue::Null)
            }
        }
    }
}

/// Convert ElementValue to a string representation for element ID generation
pub fn value_to_string(value: &ElementValue) -> String {
    match value {
        ElementValue::Integer(i) => i.to_string(),
        ElementValue::Float(f) => f.to_string(),
        ElementValue::String(s) => s.to_string(),
        ElementValue::Bool(b) => b.to_string(),
        ElementValue::Null => "null".to_string(),
        _ => format!("{value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_to_string() {
        assert_eq!(value_to_string(&ElementValue::Integer(42)), "42");
        assert_eq!(
            value_to_string(&ElementValue::String(Arc::from("test"))),
            "test"
        );
        assert_eq!(value_to_string(&ElementValue::Bool(true)), "true");
        assert_eq!(value_to_string(&ElementValue::Null), "null");
    }

    #[test]
    fn test_value_to_string_float() {
        let val = ElementValue::Float(ordered_float::OrderedFloat(2.14));
        assert_eq!(value_to_string(&val), "2.14");
    }
}
