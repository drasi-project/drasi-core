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

use anyhow::{anyhow, Result};
use base64::Engine;
use rusqlite::hooks::{Action, PreUpdateCase};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub type JsonRow = serde_json::Map<String, Value>;

#[derive(Debug)]
pub enum SqliteCommand {
    Execute {
        sql: String,
        response_tx: oneshot::Sender<Result<usize>>,
    },
    ExecuteBatch {
        sql: String,
        response_tx: oneshot::Sender<Result<()>>,
    },
    QueryRows {
        sql: String,
        response_tx: oneshot::Sender<Result<Vec<JsonRow>>>,
    },
    BeginTransaction {
        response_tx: oneshot::Sender<Result<()>>,
    },
    CommitTransaction {
        response_tx: oneshot::Sender<Result<()>>,
    },
    RollbackTransaction {
        response_tx: oneshot::Sender<Result<()>>,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct ChangeEvent {
    pub action: Action,
    pub table: String,
    pub old_values: Option<Vec<(String, Value)>>,
    pub new_values: Option<Vec<(String, Value)>>,
    pub old_row_id: Option<i64>,
    pub new_row_id: Option<i64>,
    pub table_pk_columns: Vec<String>,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone)]
pub struct SqliteThreadConfig {
    pub path: Option<String>,
    pub tables: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone)]
struct TableSchema {
    columns: Vec<String>,
    pk_columns: Vec<String>,
}

#[derive(Debug, Default)]
struct TransactionBuffer {
    events: Vec<ChangeEvent>,
    savepoint_markers: Vec<(String, usize)>,
}

impl TransactionBuffer {
    fn push(&mut self, event: ChangeEvent) {
        self.events.push(event);
    }

    fn begin_savepoint(&mut self, name: String) {
        self.savepoint_markers.push((name, self.events.len()));
    }

    fn rollback_to_savepoint(&mut self, name: &str) {
        if let Some(index) = self
            .savepoint_markers
            .iter()
            .rposition(|(marker_name, _)| marker_name.eq_ignore_ascii_case(name))
        {
            let marker = self.savepoint_markers[index].1;
            self.events.truncate(marker);
            self.savepoint_markers.truncate(index + 1);
        }
    }

    fn release_savepoint(&mut self, name: &str) {
        if let Some(index) = self
            .savepoint_markers
            .iter()
            .rposition(|(marker_name, _)| marker_name.eq_ignore_ascii_case(name))
        {
            self.savepoint_markers.remove(index);
        }
    }

    fn take_all(&mut self) -> Vec<ChangeEvent> {
        self.savepoint_markers.clear();
        std::mem::take(&mut self.events)
    }

    fn clear(&mut self) {
        self.events.clear();
        self.savepoint_markers.clear();
    }
}

#[derive(Debug, Clone)]
enum SavepointCommand {
    Savepoint(String),
    RollbackTo(String),
    Release(String),
}

pub fn run_sqlite_thread(
    config: SqliteThreadConfig,
    mut command_rx: mpsc::UnboundedReceiver<SqliteCommand>,
    event_tx: mpsc::UnboundedSender<ChangeEvent>,
) -> Result<()> {
    let conn = if let Some(path) = config.path {
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?
    } else {
        Connection::open_in_memory()?
    };

    let allowed_tables = config
        .tables
        .map(|tables| tables.into_iter().collect::<HashSet<_>>());
    let schema_cache: Arc<Mutex<HashMap<String, TableSchema>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let tx_buffer = Arc::new(Mutex::new(TransactionBuffer::default()));

    {
        let mut cache = schema_cache
            .lock()
            .map_err(|_| anyhow!("schema cache poisoned"))?;
        refresh_schema_cache(&conn, &mut cache, allowed_tables.as_ref())?;
    }

    let schema_cache_pre = Arc::clone(&schema_cache);
    let tx_buffer_pre = Arc::clone(&tx_buffer);
    let allowed_tables_pre = allowed_tables.clone();

    conn.preupdate_hook(Some(
        move |action: Action, db_name: &str, table_name: &str, case: &PreUpdateCase| {
            if db_name != "main" {
                return;
            }

            if let Some(allowed) = &allowed_tables_pre {
                if !allowed.contains(table_name) {
                    return;
                }
            }

            let schema = schema_cache_pre
                .lock()
                .ok()
                .and_then(|cache| cache.get(table_name).cloned())
                .unwrap_or_default();

            let mut old_values: Option<Vec<(String, Value)>> = None;
            let mut new_values: Option<Vec<(String, Value)>> = None;
            let mut old_row_id = None;
            let mut new_row_id = None;

            match case {
                PreUpdateCase::Insert(accessor) => {
                    let mut values = Vec::new();
                    for idx in 0..accessor.get_column_count() {
                        if let Ok(v) = accessor.get_new_column_value(idx) {
                            values.push((column_name(&schema, idx), value_ref_to_json(v)));
                        }
                    }
                    new_values = Some(values);
                    new_row_id = Some(accessor.get_new_row_id());
                }
                PreUpdateCase::Delete(accessor) => {
                    let mut values = Vec::new();
                    for idx in 0..accessor.get_column_count() {
                        if let Ok(v) = accessor.get_old_column_value(idx) {
                            values.push((column_name(&schema, idx), value_ref_to_json(v)));
                        }
                    }
                    old_values = Some(values);
                    old_row_id = Some(accessor.get_old_row_id());
                }
                PreUpdateCase::Update {
                    old_value_accessor,
                    new_value_accessor,
                } => {
                    let mut old = Vec::new();
                    let mut new = Vec::new();
                    for idx in 0..old_value_accessor.get_column_count() {
                        let name = column_name(&schema, idx);
                        if let Ok(v) = old_value_accessor.get_old_column_value(idx) {
                            old.push((name.clone(), value_ref_to_json(v)));
                        }
                        if let Ok(v) = new_value_accessor.get_new_column_value(idx) {
                            new.push((name, value_ref_to_json(v)));
                        }
                    }
                    old_values = Some(old);
                    new_values = Some(new);
                    old_row_id = Some(old_value_accessor.get_old_row_id());
                    new_row_id = Some(new_value_accessor.get_new_row_id());
                }
                PreUpdateCase::Unknown => {}
            }

            let event = ChangeEvent {
                action,
                table: table_name.to_string(),
                old_values,
                new_values,
                old_row_id,
                new_row_id,
                table_pk_columns: schema.pk_columns,
                timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
            };

            if let Ok(mut buffer) = tx_buffer_pre.lock() {
                buffer.push(event);
            }
        },
    ));

    let tx_buffer_commit = Arc::clone(&tx_buffer);
    let event_tx_commit = event_tx.clone();
    conn.commit_hook(Some(move || {
        if let Ok(mut buffer) = tx_buffer_commit.lock() {
            for event in buffer.take_all() {
                let _ = event_tx_commit.send(event);
            }
        }
        false
    }));

    let tx_buffer_rollback = Arc::clone(&tx_buffer);
    conn.rollback_hook(Some(move || {
        if let Ok(mut buffer) = tx_buffer_rollback.lock() {
            buffer.clear();
        }
    }));

    while let Some(command) = command_rx.blocking_recv() {
        match command {
            SqliteCommand::Execute { sql, response_tx } => {
                let savepoint_command = parse_savepoint_command(sql.as_str());
                let result = conn.execute(sql.as_str(), []).map_err(anyhow::Error::from);
                if result.is_ok() {
                    if let Some(command) = savepoint_command {
                        apply_savepoint_command(&tx_buffer, command);
                    }
                    let _ = refresh_schema_cache_for_runtime(
                        &conn,
                        &schema_cache,
                        allowed_tables.as_ref(),
                    );
                }
                let _ = response_tx.send(result);
            }
            SqliteCommand::ExecuteBatch { sql, response_tx } => {
                let result = conn
                    .execute_batch(sql.as_str())
                    .map_err(anyhow::Error::from);
                if result.is_ok() {
                    let _ = refresh_schema_cache_for_runtime(
                        &conn,
                        &schema_cache,
                        allowed_tables.as_ref(),
                    );
                }
                let _ = response_tx.send(result);
            }
            SqliteCommand::QueryRows { sql, response_tx } => {
                let _ = response_tx.send(query_rows(&conn, sql.as_str()));
            }
            SqliteCommand::BeginTransaction { response_tx } => {
                let result = conn
                    .execute("BEGIN TRANSACTION", [])
                    .map(|_| ())
                    .map_err(anyhow::Error::from);
                let _ = response_tx.send(result);
            }
            SqliteCommand::CommitTransaction { response_tx } => {
                let result = conn
                    .execute("COMMIT", [])
                    .map(|_| ())
                    .map_err(anyhow::Error::from);
                let _ = response_tx.send(result);
            }
            SqliteCommand::RollbackTransaction { response_tx } => {
                let result = conn
                    .execute("ROLLBACK", [])
                    .map(|_| ())
                    .map_err(anyhow::Error::from);
                let _ = response_tx.send(result);
            }
            SqliteCommand::Shutdown => break,
        }
    }

    Ok(())
}

fn refresh_schema_cache_for_runtime(
    conn: &Connection,
    schema_cache: &Arc<Mutex<HashMap<String, TableSchema>>>,
    allowed_tables: Option<&HashSet<String>>,
) -> Result<()> {
    let mut cache = schema_cache
        .lock()
        .map_err(|_| anyhow!("schema cache poisoned"))?;
    refresh_schema_cache(conn, &mut cache, allowed_tables)
}

fn refresh_schema_cache(
    conn: &Connection,
    cache: &mut HashMap<String, TableSchema>,
    allowed_tables: Option<&HashSet<String>>,
) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let table_names = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for table in table_names {
        if let Some(allowed) = allowed_tables {
            if !allowed.contains(&table) {
                continue;
            }
        }
        let schema = load_table_schema(conn, &table)?;
        cache.insert(table, schema);
    }
    Ok(())
}

fn load_table_schema(conn: &Connection, table: &str) -> Result<TableSchema> {
    let escaped = table.replace('\'', "''");
    let sql = format!("PRAGMA table_info('{escaped}')");
    let mut stmt = conn.prepare(sql.as_str())?;

    let mut columns = Vec::new();
    let mut pk_pairs: Vec<(i64, String)> = Vec::new();

    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        let pk_order: i64 = row.get(5)?;
        columns.push(name.clone());
        if pk_order > 0 {
            pk_pairs.push((pk_order, name));
        }
    }

    pk_pairs.sort_by_key(|(order, _)| *order);
    let pk_columns = pk_pairs
        .into_iter()
        .map(|(_, name)| name)
        .collect::<Vec<_>>();

    Ok(TableSchema {
        columns,
        pk_columns,
    })
}

fn column_name(schema: &TableSchema, index: i32) -> String {
    schema
        .columns
        .get(index as usize)
        .cloned()
        .unwrap_or_else(|| format!("col_{index}"))
}

fn value_ref_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => Value::Number(v.into()),
        ValueRef::Real(v) => serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(v) => Value::String(String::from_utf8_lossy(v).to_string()),
        ValueRef::Blob(v) => Value::String(base64::engine::general_purpose::STANDARD.encode(v)),
    }
}

fn query_rows(conn: &Connection, sql: &str) -> Result<Vec<JsonRow>> {
    let mut stmt = conn.prepare(sql)?;
    let columns = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();

    let mut rows = stmt.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut item = serde_json::Map::new();
        for (index, name) in columns.iter().enumerate() {
            let value = row.get_ref(index)?;
            item.insert(name.clone(), value_ref_to_json(value));
        }
        result.push(item);
    }
    Ok(result)
}

fn parse_savepoint_command(sql: &str) -> Option<SavepointCommand> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(name) = lower.strip_prefix("savepoint ") {
        return extract_identifier(trimmed, name.len()).map(SavepointCommand::Savepoint);
    }
    if let Some(name) = lower.strip_prefix("rollback to savepoint ") {
        return extract_identifier(trimmed, name.len()).map(SavepointCommand::RollbackTo);
    }
    if let Some(name) = lower.strip_prefix("rollback to ") {
        return extract_identifier(trimmed, name.len()).map(SavepointCommand::RollbackTo);
    }
    if let Some(name) = lower.strip_prefix("release savepoint ") {
        return extract_identifier(trimmed, name.len()).map(SavepointCommand::Release);
    }
    if let Some(name) = lower.strip_prefix("release ") {
        return extract_identifier(trimmed, name.len()).map(SavepointCommand::Release);
    }
    None
}

fn extract_identifier(original_sql: &str, suffix_len: usize) -> Option<String> {
    let sql = original_sql.trim().trim_end_matches(';').trim();
    let start = sql.len().checked_sub(suffix_len)?;
    let remainder = sql.get(start..)?.trim();
    let token = remainder
        .split_whitespace()
        .next()
        .map(|value| value.trim_matches('"').trim_matches('\'').to_string())?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn apply_savepoint_command(tx_buffer: &Arc<Mutex<TransactionBuffer>>, command: SavepointCommand) {
    if let Ok(mut buffer) = tx_buffer.lock() {
        match command {
            SavepointCommand::Savepoint(name) => buffer.begin_savepoint(name),
            SavepointCommand::RollbackTo(name) => buffer.rollback_to_savepoint(&name),
            SavepointCommand::Release(name) => buffer.release_savepoint(&name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    fn start_thread() -> (
        mpsc::UnboundedSender<SqliteCommand>,
        mpsc::UnboundedReceiver<ChangeEvent>,
        std::thread::JoinHandle<()>,
    ) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let join_handle = std::thread::spawn(move || {
            run_sqlite_thread(
                SqliteThreadConfig {
                    path: None,
                    tables: None,
                },
                command_rx,
                event_tx,
            )
            .expect("sqlite thread failed");
        });
        (command_tx, event_rx, join_handle)
    }

    async fn exec(command_tx: &mpsc::UnboundedSender<SqliteCommand>, sql: &str) -> Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        command_tx
            .send(SqliteCommand::Execute {
                sql: sql.to_string(),
                response_tx,
            })
            .expect("failed to send execute command");
        response_rx.await.expect("response channel closed")
    }

    async fn begin_tx(command_tx: &mpsc::UnboundedSender<SqliteCommand>) {
        let (response_tx, response_rx) = oneshot::channel();
        command_tx
            .send(SqliteCommand::BeginTransaction { response_tx })
            .expect("failed to send begin transaction");
        response_rx
            .await
            .expect("begin response closed")
            .expect("begin failed");
    }

    async fn commit_tx(command_tx: &mpsc::UnboundedSender<SqliteCommand>) {
        let (response_tx, response_rx) = oneshot::channel();
        command_tx
            .send(SqliteCommand::CommitTransaction { response_tx })
            .expect("failed to send commit transaction");
        response_rx
            .await
            .expect("commit response closed")
            .expect("commit failed");
    }

    async fn rollback_tx(command_tx: &mpsc::UnboundedSender<SqliteCommand>) {
        let (response_tx, response_rx) = oneshot::channel();
        command_tx
            .send(SqliteCommand::RollbackTransaction { response_tx })
            .expect("failed to send rollback transaction");
        response_rx
            .await
            .expect("rollback response closed")
            .expect("rollback failed");
    }

    #[tokio::test]
    async fn committed_transaction_dispatches_events() {
        let (command_tx, mut event_rx, join_handle) = start_thread();

        exec(
            &command_tx,
            "CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT)",
        )
        .await
        .expect("create table failed");

        begin_tx(&command_tx).await;
        exec(&command_tx, "INSERT INTO sensors(id, name) VALUES (1, 'a')")
            .await
            .expect("insert 1 failed");
        exec(&command_tx, "INSERT INTO sensors(id, name) VALUES (2, 'b')")
            .await
            .expect("insert 2 failed");
        commit_tx(&command_tx).await;

        let first = timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting first event")
            .expect("missing first event");
        let second = timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting second event")
            .expect("missing second event");

        assert_eq!(first.action, Action::SQLITE_INSERT);
        assert_eq!(second.action, Action::SQLITE_INSERT);

        command_tx
            .send(SqliteCommand::Shutdown)
            .expect("failed to send shutdown");
        join_handle.join().expect("thread join failed");
    }

    #[tokio::test]
    async fn rolled_back_transaction_discards_events() {
        let (command_tx, mut event_rx, join_handle) = start_thread();

        exec(
            &command_tx,
            "CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT)",
        )
        .await
        .expect("create table failed");

        begin_tx(&command_tx).await;
        exec(&command_tx, "INSERT INTO sensors(id, name) VALUES (1, 'a')")
            .await
            .expect("insert failed");
        rollback_tx(&command_tx).await;

        let receive = timeout(Duration::from_millis(200), event_rx.recv()).await;
        assert!(receive.is_err(), "expected no event after rollback");

        command_tx
            .send(SqliteCommand::Shutdown)
            .expect("failed to send shutdown");
        join_handle.join().expect("thread join failed");
    }

    #[tokio::test]
    async fn rollback_to_savepoint_discards_partial_events() {
        let (command_tx, mut event_rx, join_handle) = start_thread();

        exec(
            &command_tx,
            "CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT)",
        )
        .await
        .expect("create table failed");

        begin_tx(&command_tx).await;
        exec(&command_tx, "INSERT INTO sensors(id, name) VALUES (1, 'a')")
            .await
            .expect("insert 1 failed");
        exec(&command_tx, "SAVEPOINT s1")
            .await
            .expect("savepoint failed");
        exec(&command_tx, "INSERT INTO sensors(id, name) VALUES (2, 'b')")
            .await
            .expect("insert 2 failed");
        exec(&command_tx, "ROLLBACK TO SAVEPOINT s1")
            .await
            .expect("rollback savepoint failed");
        exec(&command_tx, "INSERT INTO sensors(id, name) VALUES (3, 'c')")
            .await
            .expect("insert 3 failed");
        commit_tx(&command_tx).await;

        let first = timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting first event")
            .expect("missing first event");
        let second = timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting second event")
            .expect("missing second event");
        let third = timeout(Duration::from_millis(200), event_rx.recv()).await;

        let first_id = first
            .new_values
            .as_ref()
            .and_then(|values| values.iter().find(|(k, _)| k == "id").map(|(_, v)| v))
            .cloned()
            .unwrap_or_default();
        let second_id = second
            .new_values
            .as_ref()
            .and_then(|values| values.iter().find(|(k, _)| k == "id").map(|(_, v)| v))
            .cloned()
            .unwrap_or_default();

        assert_eq!(first_id, serde_json::json!(1));
        assert_eq!(second_id, serde_json::json!(3));
        assert!(third.is_err(), "expected no third committed event");

        command_tx
            .send(SqliteCommand::Shutdown)
            .expect("failed to send shutdown");
        join_handle.join().expect("thread join failed");
    }
}
