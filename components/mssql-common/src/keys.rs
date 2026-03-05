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

//! Primary key discovery and element ID generation

use crate::config::{MsSqlSourceConfig, TableKeyConfig};
use crate::error::{MsSqlError, PrimaryKeyError};
use crate::types::{extract_column_value, value_to_string};
use anyhow::{anyhow, Result};
use drasi_core::models::ElementValue;
use log::warn;
use std::collections::HashMap;
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
    ///
    /// Queries sys.indexes, sys.index_columns, sys.columns, and sys.tables
    /// to find primary key columns for all tables in the database.
    pub async fn discover_keys(
        &mut self,
        client: &mut Client<Compat<TcpStream>>,
        config: &MsSqlSourceConfig,
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
        // Try exact match first
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
    ///
    /// Format: `{table_name}:{key_values}`
    ///
    /// # Arguments
    /// * `table` - Table name
    /// * `row` - Tiberius row with data
    ///
    /// # Returns
    /// Element ID string
    ///
    /// # Errors
    /// Returns an error if no primary key is configured for the table or if all
    /// primary key values are NULL. This is intentional - without a stable primary
    /// key, UPDATE and DELETE operations cannot be correctly matched to previous
    /// INSERT operations, breaking change tracking.
    pub fn generate_element_id(&self, table: &str, row: &Row) -> Result<String> {
        let keys = match self.get(table) {
            Some(keys) => keys,
            None => {
                return Err(MsSqlError::PrimaryKey(PrimaryKeyError::NotConfigured {
                    table: table.to_string(),
                })
                .into());
            }
        };

        let mut key_values = Vec::new();
        let mut null_columns = Vec::new();

        for pk_col in keys {
            // Find column index
            if let Some(col_idx) = row.columns().iter().position(|c| c.name() == pk_col) {
                let value = extract_column_value(row, col_idx)?;

                if !matches!(value, ElementValue::Null) {
                    key_values.push(value_to_string(&value));
                } else {
                    null_columns.push(pk_col.clone());
                }
            } else {
                return Err(MsSqlError::PrimaryKey(PrimaryKeyError::ColumnNotFound {
                    table: table.to_string(),
                    column: pk_col.clone(),
                })
                .into());
            }
        }

        // Generate element ID
        if !key_values.is_empty() {
            // Warn if some (but not all) key columns are NULL
            if !null_columns.is_empty() {
                warn!(
                    "NULL value(s) in primary key column(s) {null_columns:?} for table '{table}'. \
                     Using remaining key columns for element ID."
                );
            }
            Ok(format!("{}:{}", table, key_values.join("_")))
        } else {
            // All primary key values are NULL - this is an error
            Err(MsSqlError::PrimaryKey(PrimaryKeyError::AllNull {
                table: table.to_string(),
                columns: keys.clone(),
            })
            .into())
        }
    }
}

impl Default for PrimaryKeyCache {
    fn default() -> Self {
        Self::new()
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
    fn test_composite_key() {
        let mut cache = PrimaryKeyCache::new();
        cache.keys.insert(
            "order_items".to_string(),
            vec!["order_id".to_string(), "product_id".to_string()],
        );

        let keys = cache.get("order_items").unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "order_id");
        assert_eq!(keys[1], "product_id");
    }
}
