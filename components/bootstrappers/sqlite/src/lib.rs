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

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEventSender;
use log::{info, warn};
use ordered_float::OrderedFloat;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Per-table key configuration for stable element IDs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// SQLite bootstrap provider.
#[derive(Clone)]
pub struct SqliteBootstrapProvider {
    path: Option<String>,
    tables: Option<Vec<String>>,
    table_keys: Vec<TableKeyConfig>,
}

impl SqliteBootstrapProvider {
    pub fn builder() -> SqliteBootstrapBuilder {
        SqliteBootstrapBuilder::new()
    }

    fn key_columns_for_table(&self, conn: &Connection, table: &str) -> Result<Vec<String>> {
        if let Some(cfg) = self.table_keys.iter().find(|item| item.table == table) {
            return Ok(cfg.key_columns.clone());
        }
        detect_primary_key(conn, table)
    }
}

/// Builder for [`SqliteBootstrapProvider`].
pub struct SqliteBootstrapBuilder {
    path: Option<String>,
    tables: Option<Vec<String>>,
    table_keys: Vec<TableKeyConfig>,
}

impl SqliteBootstrapBuilder {
    fn new() -> Self {
        Self {
            path: None,
            tables: None,
            table_keys: Vec::new(),
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn in_memory(mut self) -> Self {
        self.path = None;
        self
    }

    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = Some(tables);
        self
    }

    pub fn with_table_keys(mut self, table_keys: Vec<TableKeyConfig>) -> Self {
        self.table_keys = table_keys;
        self
    }

    pub fn build(self) -> SqliteBootstrapProvider {
        SqliteBootstrapProvider {
            path: self.path,
            tables: self.tables,
            table_keys: self.table_keys,
        }
    }
}

#[async_trait]
impl BootstrapProvider for SqliteBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!("Starting SQLite bootstrap for query '{}'", request.query_id);

        let changes = {
            let Some(path) = &self.path else {
                warn!("SQLite bootstrap skipped for in-memory database");
                return Ok(0);
            };

            let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let tables = resolve_tables(&conn, self.tables.as_ref())?;
            let mut changes = Vec::new();

            for table in tables {
                if !request.node_labels.is_empty() && !request.node_labels.contains(&table) {
                    continue;
                }

                let key_columns = self.key_columns_for_table(&conn, &table)?;
                let rows = read_table_rows(&conn, &table)?;

                for row in rows {
                    let element_id = generate_element_id(&table, &row, &key_columns);
                    let mut properties = ElementPropertyMap::new();
                    for (name, value) in row {
                        properties.insert(&name, value);
                    }

                    let labels: Arc<[Arc<str>]> = vec![Arc::<str>::from(table.as_str())].into();
                    let element = Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new(&context.source_id, &element_id),
                            labels,
                            effective_from: chrono::Utc::now()
                                .timestamp_nanos_opt()
                                .unwrap_or_default()
                                as u64,
                        },
                        properties,
                    };

                    changes.push(SourceChange::Insert { element });
                }
            }

            changes
        };

        let mut count = 0usize;
        for change in changes {
            let event = drasi_lib::channels::BootstrapEvent {
                source_id: context.source_id.clone(),
                change,
                timestamp: chrono::Utc::now(),
                sequence: context.next_sequence(),
            };
            event_tx.send(event).await?;
            count += 1;
        }

        info!(
            "SQLite bootstrap completed for query '{}': {} rows",
            request.query_id, count
        );
        Ok(count)
    }
}

fn resolve_tables(
    conn: &Connection,
    configured_tables: Option<&Vec<String>>,
) -> Result<Vec<String>> {
    if let Some(tables) = configured_tables {
        return Ok(tables.clone());
    }

    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(tables)
}

fn read_table_rows(conn: &Connection, table: &str) -> Result<Vec<Vec<(String, ElementValue)>>> {
    let query = format!("SELECT * FROM {}", quote_ident(table));
    let mut stmt = conn.prepare(&query)?;
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|index| stmt.column_name(index).unwrap_or("").to_string())
        .collect();

    let mut rows = stmt.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(column_names.len());
        for (index, name) in column_names.iter().enumerate() {
            let value_ref = row.get_ref(index)?;
            values.push((name.clone(), value_ref_to_element_value(value_ref)));
        }
        result.push(values);
    }
    Ok(result)
}

fn detect_primary_key(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let mut stmt = conn.prepare(&sql)?;
    let mut key_pairs = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let pk: i64 = row.get(5)?;
            Ok((pk, name))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    key_pairs.retain(|(pk, _)| *pk > 0);
    key_pairs.sort_by_key(|(pk, _)| *pk);
    Ok(key_pairs.into_iter().map(|(_, name)| name).collect())
}

fn value_ref_to_element_value(value_ref: ValueRef<'_>) -> ElementValue {
    match value_ref {
        ValueRef::Null => ElementValue::Null,
        ValueRef::Integer(i) => ElementValue::Integer(i),
        ValueRef::Real(f) => ElementValue::Float(OrderedFloat(f)),
        ValueRef::Text(t) => ElementValue::String(Arc::from(String::from_utf8_lossy(t).as_ref())),
        ValueRef::Blob(b) => ElementValue::String(Arc::from(
            base64::engine::general_purpose::STANDARD.encode(b),
        )),
    }
}

fn generate_element_id(
    table: &str,
    values: &[(String, ElementValue)],
    key_columns: &[String],
) -> String {
    if key_columns.is_empty() {
        let all_values = values
            .iter()
            .map(|(_, value)| value_to_id_fragment(value))
            .collect::<Vec<_>>();
        return format!("{table}:{}", all_values.join(":"));
    }

    let key_parts = key_columns
        .iter()
        .filter_map(|column| {
            values
                .iter()
                .find(|(name, _)| name == column)
                .map(|(_, v)| v)
        })
        .map(value_to_id_fragment)
        .collect::<Vec<_>>();

    if key_parts.is_empty() {
        format!("{table}:missing-key")
    } else {
        format!("{table}:{}", key_parts.join(":"))
    }
}

fn value_to_id_fragment(value: &ElementValue) -> String {
    match value {
        ElementValue::Null => "null".to_string(),
        ElementValue::Bool(v) => v.to_string(),
        ElementValue::Float(v) => v.to_string(),
        ElementValue::Integer(v) => v.to_string(),
        ElementValue::String(v) => v.to_string(),
        ElementValue::List(v) => format!("{v:?}").replace(':', "%3A"),
        ElementValue::Object(v) => format!("{v:?}").replace(':', "%3A"),
    }
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
