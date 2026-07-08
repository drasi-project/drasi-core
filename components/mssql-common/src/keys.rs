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
    /// Map of canonical `schema.table` name -> ordered list of primary key
    /// column names. Keying by the canonical schema-qualified name keeps
    /// same-named tables in different schemas (e.g. `dbo.Orders` and
    /// `sales.Orders`) from merging their primary-key columns into one entry.
    keys: HashMap<String, Vec<String>>,
    /// Map of lowercased bare table name -> canonical `schema.table` (using the
    /// catalog's casing). Used to produce a stable element-ID prefix that is
    /// identical regardless of how the table was written in config (`Products`,
    /// `dbo.Products`, `DBO.products` all resolve to the same canonical name),
    /// so the bootstrap and CDC paths agree on element IDs.
    canonical_names: HashMap<String, String>,
}

impl PrimaryKeyCache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
            canonical_names: HashMap::new(),
        }
    }

    /// Discover primary keys from MS SQL system catalogs
    ///
    /// Queries sys.indexes, sys.index_columns, sys.columns, sys.tables, and
    /// sys.schemas to find primary key columns (and the owning schema) for all
    /// tables in the database.
    pub async fn discover_keys(
        &mut self,
        client: &mut Client<Compat<TcpStream>>,
        config: &MsSqlSourceConfig,
    ) -> Result<()> {
        let query = "
            SELECT 
                s.name AS schema_name,
                t.name AS table_name,
                c.name AS column_name,
                ic.key_ordinal
            FROM sys.indexes i
            INNER JOIN sys.index_columns ic ON i.object_id = ic.object_id 
                AND i.index_id = ic.index_id
            INNER JOIN sys.columns c ON ic.object_id = c.object_id 
                AND ic.column_id = c.column_id
            INNER JOIN sys.tables t ON i.object_id = t.object_id
            INNER JOIN sys.schemas s ON t.schema_id = s.schema_id
            WHERE i.is_primary_key = 1
            ORDER BY s.name, t.name, ic.key_ordinal
        ";

        let stream = client.query(query, &[]).await?;
        let rows = stream.into_first_result().await?;

        for row in rows {
            let schema_name: &str = row.get(0).ok_or_else(|| anyhow!("Missing schema_name"))?;
            let table_name: &str = row.get(1).ok_or_else(|| anyhow!("Missing table_name"))?;
            let column_name: &str = row.get(2).ok_or_else(|| anyhow!("Missing column_name"))?;

            let canonical = format!("{schema_name}.{table_name}");

            self.keys
                .entry(canonical.clone())
                .or_default()
                .push(column_name.to_string());

            self.canonical_names
                .entry(table_name.to_lowercase())
                .and_modify(|existing| {
                    if *existing != canonical {
                        warn!(
                            "Table '{table_name}' exists in schemas '{}' and '{schema_name}'; \
                             unqualified references will resolve to element-ID prefix '{existing}'. \
                             Use schema-qualified table names in config to disambiguate.",
                            existing.split('.').next().unwrap_or("?")
                        );
                    }
                })
                .or_insert_with(|| canonical.clone());
        }

        // Merge with configured table_keys (which take precedence). Insert by the
        // canonical `schema.table` so config lookups match discovery keying.
        for tk in &config.table_keys {
            let canonical = self.canonical_table(&tk.table);
            self.keys.insert(canonical, tk.key_columns.clone());
        }

        log::info!("Discovered primary keys for {} tables", self.keys.len());
        for (table, keys) in &self.keys {
            log::debug!("Table '{table}' primary key: {keys:?}");
        }

        Ok(())
    }

    /// Get primary key columns for a table.
    ///
    /// Resolves the reference to its canonical `schema.table` name (via
    /// [`canonical_table`](Self::canonical_table)) before looking it up, so that
    /// `Orders`, `dbo.Orders`, and `DBO.orders` all find the same entry while
    /// distinct schemas (`dbo.Orders` vs `sales.Orders`) stay separate.
    pub fn get(&self, table: &str) -> Option<&Vec<String>> {
        // Primary lookup: canonical schema.table (how keys are stored).
        if let Some(keys) = self.keys.get(&self.canonical_table(table)) {
            return Some(keys);
        }

        // Fallback: an exact, verbatim match (e.g. a caller-inserted key).
        self.keys.get(table)
    }

    /// Resolve any table reference to a canonical `schema.table` name.
    ///
    /// This makes the element-ID prefix independent of how the table was written
    /// in config, while never collapsing distinct schemas:
    /// - An **unqualified** name (`Products`) adopts the discovered catalog name
    ///   (e.g. `dbo.Products`), or defaults to `dbo.<table>` when not discovered.
    /// - A **schema-qualified** name (`dbo.Products`) adopts the catalog's casing
    ///   only when the schema matches; an explicit, non-matching schema (e.g.
    ///   `sales.Products` when the catalog discovered `dbo.Products`) is preserved
    ///   as-is so same-named tables in different schemas get distinct element IDs.
    pub fn canonical_table(&self, table: &str) -> String {
        match table.split_once('.') {
            Some((schema, tbl)) => {
                // Only adopt the catalog canonical when it is the same schema.
                if let Some(canonical) = self.canonical_names.get(&tbl.to_lowercase()) {
                    if let Some((canonical_schema, _)) = canonical.split_once('.') {
                        if canonical_schema.eq_ignore_ascii_case(schema) {
                            return canonical.clone();
                        }
                    }
                }
                // Preserve the explicit schema (different schema, or undiscovered).
                format!("{schema}.{tbl}")
            }
            None => self
                .canonical_names
                .get(&table.to_lowercase())
                .cloned()
                .unwrap_or_else(|| format!("dbo.{table}")),
        }
    }

    /// Generate element ID from a row using primary key values
    ///
    /// Format: `{canonical_schema.table}:{key_values}`
    ///
    /// The table portion is canonicalized via [`canonical_table`](Self::canonical_table)
    /// so that the bootstrap and CDC paths produce identical element IDs for the
    /// same row even when configured with differently-formatted table names.
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
            Ok(format!(
                "{}:{}",
                self.canonical_table(table),
                key_values.join("_")
            ))
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
            .insert("dbo.orders".to_string(), vec!["order_id".to_string()]);
        cache
            .canonical_names
            .insert("orders".to_string(), "dbo.orders".to_string());

        // Bare, same-schema, and differently-cased references all resolve.
        assert_eq!(cache.get("orders").unwrap(), &vec!["order_id"]);
        assert_eq!(cache.get("dbo.orders").unwrap(), &vec!["order_id"]);
        assert_eq!(cache.get("DBO.Orders").unwrap(), &vec!["order_id"]);
    }

    #[test]
    fn test_composite_key() {
        let mut cache = PrimaryKeyCache::new();
        cache.keys.insert(
            "dbo.order_items".to_string(),
            vec!["order_id".to_string(), "product_id".to_string()],
        );
        cache
            .canonical_names
            .insert("order_items".to_string(), "dbo.order_items".to_string());

        let keys = cache.get("order_items").unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "order_id");
        assert_eq!(keys[1], "product_id");
    }

    #[test]
    fn test_same_table_name_across_schemas_stays_distinct() {
        // Regression: `dbo.Orders` and `sales.Orders` must not merge their PK
        // column lists into a single bare-name entry.
        let mut cache = PrimaryKeyCache::new();
        cache
            .keys
            .insert("dbo.Orders".to_string(), vec!["OrderId".to_string()]);
        cache
            .keys
            .insert("sales.Orders".to_string(), vec!["SaleId".to_string()]);
        // The catalog discovered `dbo.Orders` first, so the bare name resolves there.
        cache
            .canonical_names
            .insert("orders".to_string(), "dbo.Orders".to_string());

        assert_eq!(cache.get("dbo.Orders").unwrap(), &vec!["OrderId"]);
        assert_eq!(cache.get("sales.Orders").unwrap(), &vec!["SaleId"]);
        // The unqualified reference follows the discovered canonical schema.
        assert_eq!(cache.get("Orders").unwrap(), &vec!["OrderId"]);
    }

    #[test]
    fn test_canonical_table_uses_catalog() {
        let mut cache = PrimaryKeyCache::new();
        cache
            .canonical_names
            .insert("products".to_string(), "dbo.Products".to_string());

        // Unqualified and same-schema inputs resolve to the catalog casing.
        assert_eq!(cache.canonical_table("Products"), "dbo.Products");
        assert_eq!(cache.canonical_table("dbo.Products"), "dbo.Products");
        assert_eq!(cache.canonical_table("DBO.products"), "dbo.Products");
        // An explicit, different schema is preserved (no cross-schema collapse).
        assert_eq!(cache.canonical_table("sales.products"), "sales.products");
    }

    #[test]
    fn test_canonical_table_fallback_without_catalog() {
        let cache = PrimaryKeyCache::new();

        // Bare and schema-qualified inputs both default to a `dbo.`-qualified
        // canonical name, so bootstrap and CDC still agree.
        assert_eq!(cache.canonical_table("Products"), "dbo.Products");
        assert_eq!(cache.canonical_table("dbo.Products"), "dbo.Products");
        // An explicit non-default schema is preserved.
        assert_eq!(cache.canonical_table("sales.Orders"), "sales.Orders");
    }
}
