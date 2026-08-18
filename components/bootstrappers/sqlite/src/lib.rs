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

#![allow(unexpected_cfgs)]

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::BootstrapEventSender;
use log::{info, warn};
use ordered_float::OrderedFloat;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use std::sync::Arc;

pub mod descriptor;

/// Per-table key configuration for stable element IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    ) -> Result<BootstrapResult> {
        info!("Starting SQLite bootstrap for query '{}'", request.query_id);

        let Some(path) = self.path.clone() else {
            warn!("SQLite bootstrap skipped for in-memory database");
            return Ok(BootstrapResult::default());
        };

        let configured_tables = self.tables.clone();
        let table_keys = self.table_keys.clone();
        let context = context.clone();
        let query_id = request.query_id.clone();

        let count = tokio::task::spawn_blocking(move || {
            stream_sqlite_tables(
                &path,
                configured_tables.as_ref(),
                &table_keys,
                &request,
                &context,
                &event_tx,
            )
        })
        .await??;

        info!("SQLite bootstrap completed for query '{query_id}': {count} rows");
        Ok(BootstrapResult {
            event_count: count,
            ..Default::default()
        })
    }
}

fn stream_sqlite_tables(
    path: &str,
    configured_tables: Option<&Vec<String>>,
    table_keys: &[TableKeyConfig],
    request: &BootstrapRequest,
    context: &BootstrapContext,
    event_tx: &BootstrapEventSender,
) -> Result<usize> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tables = resolve_tables(&conn, configured_tables)?;
    let mut count = 0usize;

    for table in tables {
        if !request.node_labels.is_empty() && !request.node_labels.contains(&table) {
            continue;
        }

        let key_columns = key_columns_for_table(&conn, table_keys, &table)?;
        let query = format!("SELECT rowid, * FROM {}", quote_ident(&table));
        let mut stmt = conn.prepare(&query)?;
        let column_names = user_column_names(&stmt);
        let mut rows = stmt.query([])?;

        while let Some(row) = rows.next()? {
            let (values, rowid) = map_query_row(row, &column_names)?;
            let element_id = generate_element_id(&table, &values, &key_columns, Some(rowid));
            let mut properties = ElementPropertyMap::new();
            for (name, value) in values {
                properties.insert(&name, value);
            }

            let labels: Arc<[Arc<str>]> = vec![Arc::<str>::from(table.as_str())].into();
            let element = Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(&context.source_id, &element_id),
                    labels,
                    effective_from: chrono::Utc::now().timestamp_millis() as u64,
                },
                properties,
            };

            event_tx
                .blocking_send(drasi_lib::channels::BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change: SourceChange::Insert { element },
                    timestamp: chrono::Utc::now(),
                    sequence: context.next_sequence(),
                })
                .map_err(|e| anyhow::anyhow!("Failed to send SQLite bootstrap event: {e}"))?;
            count += 1;
        }
    }

    Ok(count)
}

fn key_columns_for_table(
    conn: &Connection,
    table_keys: &[TableKeyConfig],
    table: &str,
) -> Result<Vec<String>> {
    if let Some(cfg) = table_keys.iter().find(|item| item.table == table) {
        return Ok(cfg.key_columns.clone());
    }
    detect_primary_key(conn, table)
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

fn user_column_names(stmt: &rusqlite::Statement<'_>) -> Vec<String> {
    // First column is rowid, remaining are user columns
    (1..stmt.column_count())
        .map(|index| stmt.column_name(index).unwrap_or("").to_string())
        .collect()
}

fn map_query_row(
    row: &rusqlite::Row<'_>,
    column_names: &[String],
) -> Result<(Vec<(String, ElementValue)>, i64)> {
    let rowid: i64 = row.get(0)?;
    let mut values = Vec::with_capacity(column_names.len());
    for (i, name) in column_names.iter().enumerate() {
        let value_ref = row.get_ref(i + 1)?;
        values.push((name.clone(), value_ref_to_element_value(value_ref)));
    }
    Ok((values, rowid))
}

fn read_table_rows(
    conn: &Connection,
    table: &str,
) -> Result<Vec<(Vec<(String, ElementValue)>, i64)>> {
    let query = format!("SELECT rowid, * FROM {}", quote_ident(table));
    let mut stmt = conn.prepare(&query)?;
    let column_names = user_column_names(&stmt);
    let mut rows = stmt.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(map_query_row(row, &column_names)?);
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
    rowid: Option<i64>,
) -> String {
    if !key_columns.is_empty() {
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

        if !key_parts.is_empty() {
            return format!("{table}:{}", key_parts.join(":"));
        }
    }

    // Fall back to rowid, matching the source's CDC behavior
    if let Some(id) = rowid {
        return format!("{table}:{id}");
    }

    format!("{table}:unknown")
}

fn value_to_id_fragment(value: &ElementValue) -> String {
    match value {
        ElementValue::Null => "null".to_string(),
        ElementValue::Bool(v) => v.to_string(),
        ElementValue::Float(v) => v.to_string(),
        ElementValue::Integer(v) => v.to_string(),
        ElementValue::String(v) => v.to_string(),
        ElementValue::LocalDateTime(v) => v.to_string(),
        ElementValue::ZonedDateTime(v) => v.to_rfc3339(),
        ElementValue::List(v) => format!("{v:?}").replace(':', "%3A"),
        ElementValue::Object(v) => format!("{v:?}").replace(':', "%3A"),
    }
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_element_id_uses_key_columns_when_provided() {
        let values = vec![
            ("id".to_string(), ElementValue::Integer(42)),
            ("name".to_string(), ElementValue::String(Arc::from("test"))),
        ];
        let keys = vec!["id".to_string()];
        assert_eq!(
            generate_element_id("sensors", &values, &keys, Some(1)),
            "sensors:42"
        );
    }

    #[test]
    fn generate_element_id_uses_composite_keys() {
        let values = vec![
            ("tenant".to_string(), ElementValue::String(Arc::from("t1"))),
            (
                "event_id".to_string(),
                ElementValue::String(Arc::from("e1")),
            ),
        ];
        let keys = vec!["tenant".to_string(), "event_id".to_string()];
        assert_eq!(
            generate_element_id("events", &values, &keys, Some(99)),
            "events:t1:e1"
        );
    }

    #[test]
    fn generate_element_id_falls_back_to_rowid_when_no_keys() {
        let values = vec![
            ("name".to_string(), ElementValue::String(Arc::from("test"))),
            ("value".to_string(), ElementValue::Integer(100)),
        ];
        let keys: Vec<String> = vec![];
        assert_eq!(
            generate_element_id("data", &values, &keys, Some(7)),
            "data:7"
        );
    }

    #[test]
    fn generate_element_id_returns_unknown_without_keys_or_rowid() {
        let values = vec![("x".to_string(), ElementValue::Integer(1))];
        let keys: Vec<String> = vec![];
        assert_eq!(
            generate_element_id("data", &values, &keys, None),
            "data:unknown"
        );
    }

    #[test]
    fn stream_sqlite_tables_filters_labels_and_assigns_sequences() {
        let path = std::env::temp_dir().join(format!(
            "drasi-sqlite-stream-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let path_str = path.to_string_lossy().to_string();
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch(
            "CREATE TABLE sensors (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO sensors VALUES (1, 'alpha');
             INSERT INTO sensors VALUES (2, 'beta');
             CREATE TABLE ignored (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO ignored VALUES (9, 'skip');",
        )
        .expect("setup");
        drop(conn);

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let context = BootstrapContext::new_minimal("server".to_string(), "src".to_string());
        let request = BootstrapRequest {
            query_id: "q1".to_string(),
            node_labels: vec!["sensors".to_string()],
            relation_labels: Vec::new(),
            request_id: "req-1".to_string(),
        };
        let tables = vec!["sensors".to_string(), "ignored".to_string()];
        let count = stream_sqlite_tables(&path_str, Some(&tables), &[], &request, &context, &tx)
            .expect("stream");
        drop(tx);

        assert_eq!(count, 2, "only sensors rows should be streamed");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let _ = std::fs::remove_file(&path);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source_id, "src");
        assert_eq!(events[0].sequence, 0);
        assert_eq!(events[1].sequence, 1);

        let names: Vec<String> = events
            .iter()
            .map(|event| match &event.change {
                SourceChange::Insert { element } => match element {
                    Element::Node { properties, .. } => match &properties["name"] {
                        ElementValue::String(name) => name.to_string(),
                        other => panic!("unexpected name value: {other:?}"),
                    },
                    other => panic!("expected node, got {other:?}"),
                },
                other => panic!("expected insert, got {other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn read_table_rows_returns_rows_with_rowid() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE items (name TEXT, value INTEGER); INSERT INTO items VALUES ('a', 1); INSERT INTO items VALUES ('b', 2);")
            .expect("setup");

        let rows = read_table_rows(&conn, "items").expect("read");
        assert_eq!(rows.len(), 2);

        let (first_row, first_rowid) = &rows[0];
        assert_eq!(*first_rowid, 1);
        assert_eq!(first_row.len(), 2);
        assert_eq!(first_row[0].0, "name");

        let (second_row, second_rowid) = &rows[1];
        assert_eq!(*second_rowid, 2);
        assert_eq!(second_row.len(), 2);
        let _ = second_row;
    }

    #[test]
    fn detect_primary_key_finds_pk_columns() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE sensors (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("setup");

        let pks = detect_primary_key(&conn, "sensors").expect("detect");
        assert_eq!(pks, vec!["id".to_string()]);
    }

    #[test]
    fn detect_primary_key_returns_empty_for_no_pk() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE data (x TEXT, y TEXT)")
            .expect("setup");

        let pks = detect_primary_key(&conn, "data").expect("detect");
        assert!(pks.is_empty());
    }

    #[test]
    fn element_id_matches_between_bootstrap_and_source_with_pk() {
        // Verifies that bootstrap and source produce the same element ID
        // when a table has a declared PK.
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE sensors (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO sensors VALUES (42, 'test');")
            .expect("setup");

        let pks = detect_primary_key(&conn, "sensors").expect("detect pk");
        let rows = read_table_rows(&conn, "sensors").expect("read");
        let (row, rowid) = &rows[0];

        let bootstrap_id = generate_element_id("sensors", row, &pks, Some(*rowid));
        // Source would produce "sensors:42" via PK column "id"
        assert_eq!(bootstrap_id, "sensors:42");
    }

    #[test]
    fn element_id_matches_between_bootstrap_and_source_without_pk() {
        // Verifies that bootstrap falls back to rowid just like source does
        // when no PK is declared.
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE data (x TEXT, y TEXT); INSERT INTO data VALUES ('a', 'b');",
        )
        .expect("setup");

        let pks = detect_primary_key(&conn, "data").expect("detect pk");
        assert!(pks.is_empty());

        let rows = read_table_rows(&conn, "data").expect("read");
        let (row, rowid) = &rows[0];

        let bootstrap_id = generate_element_id("data", row, &pks, Some(*rowid));
        // Source would produce "data:1" via rowid fallback
        assert_eq!(bootstrap_id, "data:1");
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "sqlite-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::SqliteBootstrapDescriptor],
);
