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

//! Primary key discovery and element ID generation for bootstrap

use crate::config::{MsSqlBootstrapConfig, TableKeyConfig};
use anyhow::{anyhow, Result};
use drasi_core::models::ElementValue;
use log::warn;
use ordered_float::OrderedFloat;
use std::collections::HashMap;
use std::sync::Arc;
use tiberius::{Client, Row};
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

/// Cache of primary keys for tables
pub struct PrimaryKeyCache {
    /// Map of table name -> ordered list of primary key column names
    keys: HashMap<String, Vec<String>>,
}

impl PrimaryKeyCache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
        }
    }

    /// Discover primary keys from MS SQL system catalogs
    pub async fn discover_keys(
        &mut self,
        client: &mut Client<Compat<TcpStream>>,
        config: &MsSqlBootstrapConfig,
    ) -> Result<()> {
        let query = "
            SELECT 
                t.name AS table_name,
                c.name AS column_name,
                ic.key_ordinal
            FROM sys.indexes i
            INNER JOIN sys.index_columns ic ON i.object_id = ic.object_id 
                AND i.index_id = ic.index_id
            INNER JOIN sys.columns c ON ic.object_id = c.object_id 
                AND ic.column_id = c.column_id
            INNER JOIN sys.tables t ON i.object_id = t.object_id
            WHERE i.is_primary_key = 1
            ORDER BY t.name, ic.key_ordinal
        ";

        let stream = client.query(query, &[]).await?;
        let rows = stream.into_first_result().await?;

        for row in rows {
            let table_name: &str = row.get(0).ok_or_else(|| anyhow!("Missing table_name"))?;
            let column_name: &str = row.get(1).ok_or_else(|| anyhow!("Missing column_name"))?;

            self.keys
                .entry(table_name.to_string())
                .or_default()
                .push(column_name.to_string());
        }

        // Merge with configured table_keys (which take precedence)
        for tk in &config.table_keys {
            self.keys.insert(tk.table.clone(), tk.key_columns.clone());
        }

        log::info!("Discovered primary keys for {} tables", self.keys.len());
        for (table, keys) in &self.keys {
            log::debug!("Table '{table}' primary key: {keys:?}");
        }

        Ok(())
    }

    /// Get primary key columns for a table
    /// Handles both "table" and "schema.table" formats
    pub fn get(&self, table: &str) -> Option<&Vec<String>> {
        if let Some(keys) = self.keys.get(table) {
            return Some(keys);
        }

        // Try without schema prefix (e.g., "dbo.Orders" -> "Orders")
        if let Some(table_only) = table.split('.').nth(1) {
            if let Some(keys) = self.keys.get(table_only) {
                return Some(keys);
            }
        }

        None
    }

    /// Generate element ID from a row using primary key values
    pub fn generate_element_id(&self, table: &str, row: &Row) -> Result<String> {
        let keys = match self.get(table) {
            Some(keys) => keys,
            None => {
                return Err(anyhow!(
                    "No primary key configured for table '{table}'. \
                     Add a 'table_keys' configuration entry to specify the primary key columns."
                ));
            }
        };

        let mut key_values = Vec::new();
        let mut null_columns = Vec::new();

        for pk_col in keys {
            if let Some(col_idx) = row.columns().iter().position(|c| c.name() == pk_col) {
                let value = extract_column_value(row, col_idx)?;

                if !matches!(value, ElementValue::Null) {
                    key_values.push(value_to_string(&value));
                } else {
                    null_columns.push(pk_col.clone());
                }
            } else {
                return Err(anyhow!(
                    "Primary key column '{pk_col}' not found in row for table '{table}'."
                ));
            }
        }

        if !key_values.is_empty() {
            if !null_columns.is_empty() {
                warn!(
                    "NULL value(s) in primary key column(s) {null_columns:?} for table '{table}'. \
                     Using remaining key columns for element ID."
                );
            }
            Ok(format!("{}:{}", table, key_values.join("_")))
        } else {
            Err(anyhow!(
                "All primary key values are NULL for table '{table}' (columns: {keys:?}). \
                 Cannot generate a stable element ID."
            ))
        }
    }
}

impl Default for PrimaryKeyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract a column value from a row by index and convert to ElementValue
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
                Ok(ElementValue::Float(OrderedFloat(val as f64)))
            } else if let Ok(Some(val)) = row.try_get::<f64, _>(col_idx) {
                Ok(ElementValue::Float(OrderedFloat(val)))
            } else {
                Ok(ElementValue::Null)
            }
        }
        ColumnType::Numericn | ColumnType::Decimaln => {
            if let Ok(Some(d)) = row.try_get::<rust_decimal::Decimal, _>(col_idx) {
                let f = d.to_string().parse::<f64>().unwrap_or(0.0);
                Ok(ElementValue::Float(OrderedFloat(f)))
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
            } else if let Ok(Some(val)) =
                row.try_get::<chrono::DateTime<chrono::Utc>, _>(col_idx)
            {
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
            if let Ok(Some(val)) = row.try_get::<&str, _>(col_idx) {
                Ok(ElementValue::String(Arc::from(val)))
            } else {
                warn!(
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
    fn test_empty_cache() {
        let cache = PrimaryKeyCache::new();
        assert!(cache.get("orders").is_none());
    }

    #[test]
    fn test_insert_and_get() {
        let mut cache = PrimaryKeyCache::new();
        cache
            .keys
            .insert("orders".to_string(), vec!["order_id".to_string()]);

        assert_eq!(cache.get("orders").unwrap(), &vec!["order_id"]);
    }

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
}
