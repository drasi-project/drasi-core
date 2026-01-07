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
use drasi_core::models::{ElementPropertyMap, ElementValue};
use std::sync::Arc;
use tiberius::Row;

/// Extract all properties from a CDC row, skipping metadata columns
///
/// # Arguments
/// * `row` - The Tiberius row containing both data and CDC metadata columns
///
/// # Returns
/// ElementPropertyMap with all non-CDC columns converted to ElementValue
pub fn extract_properties_from_cdc_row(row: &Row) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();

    for (idx, column) in row.columns().iter().enumerate() {
        let col_name = column.name();

        // Skip CDC metadata columns (those starting with __$)
        if crate::decoder::cdc_columns::is_metadata_column(col_name) {
            continue;
        }

        // Extract value and convert to ElementValue
        let value = extract_column_value(row, idx)?;
        properties.insert(col_name, value);
    }

    Ok(properties)
}

/// Extract a column value from a row by index and convert to ElementValue
///
/// This tries each type in sequence since tiberius requires knowing the exact type.
pub fn extract_column_value(row: &Row, col_idx: usize) -> Result<ElementValue> {
    // Try string types first (most common)
    if let Ok(val) = row.try_get::<&str, _>(col_idx) {
        return Ok(match val {
            Some(s) => ElementValue::String(Arc::from(s)),
            None => ElementValue::Null,
        });
    }
    
    // Try integer types
    if let Ok(val) = row.try_get::<i32, _>(col_idx) {
        return Ok(match val {
            Some(i) => ElementValue::Integer(i as i64),
            None => ElementValue::Null,
        });
    }
    
    if let Ok(val) = row.try_get::<i64, _>(col_idx) {
        return Ok(match val {
            Some(i) => ElementValue::Integer(i),
            None => ElementValue::Null,
        });
    }
    
    if let Ok(val) = row.try_get::<i16, _>(col_idx) {
        return Ok(match val {
            Some(i) => ElementValue::Integer(i as i64),
            None => ElementValue::Null,
        });
    }
    
    if let Ok(val) = row.try_get::<u8, _>(col_idx) {
        return Ok(match val {
            Some(i) => ElementValue::Integer(i as i64),
            None => ElementValue::Null,
        });
    }
    
    // Try boolean
    if let Ok(val) = row.try_get::<bool, _>(col_idx) {
        return Ok(match val {
            Some(b) => ElementValue::Bool(b),
            None => ElementValue::Null,
        });
    }
    
    // Try float types
    if let Ok(val) = row.try_get::<f64, _>(col_idx) {
        return Ok(match val {
            Some(f) => ElementValue::Float(ordered_float::OrderedFloat(f)),
            None => ElementValue::Null,
        });
    }
    
    if let Ok(val) = row.try_get::<f32, _>(col_idx) {
        return Ok(match val {
            Some(f) => ElementValue::Float(ordered_float::OrderedFloat(f as f64)),
            None => ElementValue::Null,
        });
    }
    
    // Try decimal/numeric
    if let Ok(val) = row.try_get::<rust_decimal::Decimal, _>(col_idx) {
        return Ok(match val {
            Some(d) => {
                let f = d.to_string().parse::<f64>().unwrap_or(0.0);
                ElementValue::Float(ordered_float::OrderedFloat(f))
            }
            None => ElementValue::Null,
        });
    }
    
    // Try UUID
    if let Ok(val) = row.try_get::<uuid::Uuid, _>(col_idx) {
        return Ok(match val {
            Some(u) => ElementValue::String(Arc::from(u.to_string())),
            None => ElementValue::Null,
        });
    }
    
    // Try datetime types - convert to string
    if let Ok(val) = row.try_get::<chrono::NaiveDateTime, _>(col_idx) {
        return Ok(match val {
            Some(dt) => ElementValue::String(Arc::from(
                dt.format("%Y-%m-%dT%H:%M:%S%.3f").to_string(),
            )),
            None => ElementValue::Null,
        });
    }
    
    // Try binary data
    if let Ok(val) = row.try_get::<&[u8], _>(col_idx) {
        return Ok(match val {
            Some(bytes) => ElementValue::String(Arc::from(format!("0x{}", hex::encode(bytes)))),
            None => ElementValue::Null,
        });
    }
    
    // If nothing matched, return Null
    Ok(ElementValue::Null)
}

/// Convert ElementValue to a string representation for element ID generation
pub fn value_to_string(value: &ElementValue) -> String {
    match value {
        ElementValue::Integer(i) => i.to_string(),
        ElementValue::Float(f) => f.to_string(),
        ElementValue::String(s) => s.to_string(),
        ElementValue::Bool(b) => b.to_string(),
        ElementValue::Null => "null".to_string(),
        _ => format!("{:?}", value),
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
        let val = ElementValue::Float(ordered_float::OrderedFloat(3.14));
        assert_eq!(value_to_string(&val), "3.14");
    }
}

