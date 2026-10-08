// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    handle::{Command, TransactionCommand},
    *,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use rusqlite::{
    hooks::{AuthAction, AuthContext, Authorization, PreUpdateCase, TransactionOperation},
    limits::Limit,
    types::{Value, ValueRef},
    Batch, Connection, Statement,
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    sync::{atomic::Ordering, Mutex},
};
use tokio::time::Instant;

#[derive(Clone, Debug)]
struct Failure(Arc<anyhow::Error>);
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

#[derive(Clone)]
struct Table {
    columns: Vec<String>,
    keys: Vec<String>,
}

#[derive(Clone)]
enum Savepoint {
    Begin(String),
    Rollback(String),
    Release(String),
}

#[derive(Default)]
struct AuthorizationState {
    internal: bool,
    scoped: bool,
    savepoint: Option<Savepoint>,
}

struct Capture {
    source: ComponentId,
    tables: BTreeMap<String, Table>,
    changes: Vec<SourceChange>,
    bytes: u64,
    markers: Vec<(String, usize, u64)>,
    failure: Option<anyhow::Error>,
    limits: SourceTransactionLimits,
    deadline: Option<Instant>,
    timestamp: DateTime<Utc>,
}

impl Capture {
    fn push(&mut self, change: SourceChange) -> Result<()> {
        anyhow::ensure!(
            self.changes.len() < self.limits.max_changes.get(),
            "SQLite transaction change limit exceeded"
        );
        let bytes = self
            .bytes
            .checked_add(bincode::serialized_size(&change)?)
            .context("SQLite transaction byte count overflow")?;
        anyhow::ensure!(
            bytes <= self.limits.max_bytes.get() as u64,
            "SQLite transaction byte limit exceeded"
        );
        self.bytes = bytes;
        self.changes.push(change);
        Ok(())
    }
    fn clear(&mut self) {
        self.changes.clear();
        self.bytes = 0;
        self.markers.clear();
        self.failure = None;
        self.deadline = None;
    }
    fn savepoint(&mut self, command: Savepoint) -> Result<()> {
        let (name, release) = match command {
            Savepoint::Begin(name) => {
                anyhow::ensure!(
                    self.markers.len() < self.limits.max_changes.get(),
                    "SQLite savepoint limit exceeded"
                );
                self.markers.push((name, self.changes.len(), self.bytes));
                return Ok(());
            }
            Savepoint::Rollback(name) => (name, false),
            Savepoint::Release(name) => (name, true),
        };
        let index = self
            .markers
            .iter()
            .rposition(|(marker, _, _)| marker.eq_ignore_ascii_case(&name))
            .context("SQLite savepoint capture is inconsistent")?;
        if release {
            self.markers.truncate(index);
        } else {
            self.changes.truncate(self.markers[index].1);
            self.bytes = self.markers[index].2;
            self.markers.truncate(index + 1);
        }
        Ok(())
    }
    fn capture(&mut self, name: &str, case: &PreUpdateCase) -> Result<()> {
        let table = self
            .tables
            .get(name)
            .context("SQLite write has no validated table schema")?
            .clone();
        let source = self.source.as_str();
        let time = u64::try_from(self.timestamp.timestamp_millis())?;
        let mut old = None;
        let mut new = None;
        match case {
            PreUpdateCase::Insert(row) => {
                new = Some(element(
                    source,
                    name,
                    &table,
                    time,
                    row.get_column_count(),
                    |i| row.get_new_column_value(i),
                )?);
            }
            PreUpdateCase::Delete(row) => {
                old = Some(element(
                    source,
                    name,
                    &table,
                    time,
                    row.get_column_count(),
                    |i| row.get_old_column_value(i),
                )?);
            }
            PreUpdateCase::Update {
                old_value_accessor: before,
                new_value_accessor: after,
            } => {
                old = Some(element(
                    source,
                    name,
                    &table,
                    time,
                    before.get_column_count(),
                    |i| before.get_old_column_value(i),
                )?);
                new = Some(element(
                    source,
                    name,
                    &table,
                    time,
                    after.get_column_count(),
                    |i| after.get_new_column_value(i),
                )?);
            }
            PreUpdateCase::Unknown => anyhow::bail!("unknown SQLite preupdate operation"),
        }
        match (old, new) {
            (None, Some(element)) => self.push(SourceChange::Insert { element })?,
            (Some(before), None) => self.push(SourceChange::Delete {
                metadata: before.get_metadata().clone(),
            })?,
            (Some(before), Some(after)) => {
                if before.get_reference() != after.get_reference() {
                    self.push(SourceChange::Delete {
                        metadata: before.get_metadata().clone(),
                    })?;
                    self.push(SourceChange::Insert { element: after })?;
                } else {
                    self.push(SourceChange::Update { element: after })?;
                }
            }
            (None, None) => anyhow::bail!("empty SQLite row change"),
        }
        Ok(())
    }
}

fn json(value: ValueRef<'_>) -> Result<serde_json::Value> {
    use base64::Engine;
    Ok(match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(value) => value.into(),
        ValueRef::Real(value) => serde_json::Number::from_f64(value)
            .context("non-finite SQLite real")?
            .into(),
        ValueRef::Text(value) => std::str::from_utf8(value)?.into(),
        ValueRef::Blob(value) => base64::engine::general_purpose::STANDARD
            .encode(value)
            .into(),
    })
}

fn element<'a>(
    source: &str,
    table_name: &str,
    table: &Table,
    time: u64,
    count: i32,
    mut value: impl FnMut(i32) -> rusqlite::Result<ValueRef<'a>>,
) -> Result<Element> {
    anyhow::ensure!(
        usize::try_from(count)? == table.columns.len(),
        "SQLite capture schema changed"
    );
    anyhow::ensure!(
        !table.keys.is_empty(),
        "native SQLite rows require a primary key"
    );
    let mut properties = ElementPropertyMap::new();
    let mut keys = BTreeMap::new();
    for (index, name) in table.columns.iter().enumerate() {
        let raw = value(i32::try_from(index)?)?;
        let kind = match raw {
            ValueRef::Null => "null",
            ValueRef::Integer(_) => "integer",
            ValueRef::Real(_) => "real",
            ValueRef::Text(_) => "text",
            ValueRef::Blob(_) => "blob",
        };
        let value = json(raw)?;
        if table.keys.contains(name) {
            anyhow::ensure!(!value.is_null(), "native SQLite primary key cannot be null");
            keys.insert(name, (kind, value.clone()));
        }
        properties.insert(name, crate::convert::json_value_to_element_value(&value));
    }
    let id = format!("sqlite:v1:{}", serde_json::to_string(&(table_name, keys))?);
    Ok(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, &id),
            labels: Arc::from([Arc::from(table_name)]),
            effective_from: time,
        },
        properties,
    })
}

struct Database {
    connection: Connection,
    config: SqliteConfig,
    capture: Arc<Mutex<Capture>>,
    authorization: Arc<Mutex<AuthorizationState>>,
    journal: Option<super::journal::Journal>,
    fenced: bool,
    schema_version: Cell<Option<i64>>,
}

impl Database {
    fn open(
        config: SqliteConfig,
        source: ComponentId,
        shutdown: watch::Receiver<bool>,
    ) -> Result<Self> {
        let connection = match &config.path {
            Some(path) => Connection::open(path)?,
            None => Connection::open_in_memory()?,
        };
        connection.busy_timeout(config.transactions.duration())?;
        connection.set_limit(
            Limit::SQLITE_LIMIT_SQL_LENGTH,
            i32::try_from(config.max_sql_bytes.get())?,
        );
        connection.set_limit(
            Limit::SQLITE_LIMIT_LENGTH,
            i32::try_from(config.transactions.max_bytes.get())?,
        );
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA recursive_triggers=ON;")?;
        if config.replay.is_some() {
            let file: String = connection.query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                !file.is_empty(),
                "native SQLite replay requires an actual persistent database file"
            );
            connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA locking_mode=EXCLUSIVE; PRAGMA synchronous=FULL; BEGIN EXCLUSIVE; COMMIT;")?;
        } else {
            let has_journal: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE lower(substr(name,1,8))='__drasi_')",
                [], |row| row.get(0),
            )?;
            anyhow::ensure!(
                !has_journal,
                "native SQLite cannot disable replay on a journal-owned database"
            );
        }
        let capture = Arc::new(Mutex::new(Capture {
            source,
            tables: BTreeMap::new(),
            changes: Vec::new(),
            bytes: 0,
            markers: Vec::new(),
            failure: None,
            limits: config.transactions,
            deadline: None,
            timestamp: Utc::now(),
        }));
        let authorization = Arc::new(Mutex::new(AuthorizationState::default()));
        let auth = authorization.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            let Ok(mut state) = auth.lock() else {
                return Authorization::Deny;
            };
            if state.internal {
                return Authorization::Allow;
            }
            let reserved = |name: &str| name.to_ascii_lowercase().starts_with("__drasi_");
            match context.action {
                AuthAction::Transaction { .. }
                | AuthAction::Pragma { .. }
                | AuthAction::Attach { .. }
                | AuthAction::Detach { .. }
                | AuthAction::AlterTable { .. }
                | AuthAction::DropTable { .. }
                | AuthAction::CreateVtable { .. }
                | AuthAction::DropVtable { .. }
                | AuthAction::CreateTempTable { .. }
                | AuthAction::DropTempTable { .. }
                | AuthAction::CreateTempIndex { .. }
                | AuthAction::DropTempIndex { .. }
                | AuthAction::CreateTempTrigger { .. }
                | AuthAction::DropTempTrigger { .. }
                | AuthAction::CreateTempView { .. }
                | AuthAction::DropTempView { .. } => Authorization::Deny,
                AuthAction::Savepoint {
                    operation,
                    savepoint_name,
                } => {
                    if !state.scoped {
                        return Authorization::Deny;
                    }
                    state.savepoint = match operation {
                        TransactionOperation::Begin => {
                            Some(Savepoint::Begin(savepoint_name.into()))
                        }
                        TransactionOperation::Rollback => {
                            Some(Savepoint::Rollback(savepoint_name.into()))
                        }
                        TransactionOperation::Release => {
                            Some(Savepoint::Release(savepoint_name.into()))
                        }
                        _ => return Authorization::Deny,
                    };
                    Authorization::Allow
                }
                AuthAction::Read { table_name, .. }
                | AuthAction::Insert { table_name }
                | AuthAction::Update { table_name, .. }
                | AuthAction::Delete { table_name }
                | AuthAction::CreateTable { table_name }
                | AuthAction::Analyze { table_name } => {
                    if reserved(table_name) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                AuthAction::CreateIndex {
                    index_name,
                    table_name,
                }
                | AuthAction::DropIndex {
                    index_name,
                    table_name,
                } => {
                    if reserved(index_name) || reserved(table_name) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                AuthAction::CreateTrigger {
                    trigger_name,
                    table_name,
                }
                | AuthAction::DropTrigger {
                    trigger_name,
                    table_name,
                } => {
                    if reserved(trigger_name) || reserved(table_name) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                AuthAction::CreateView { view_name } | AuthAction::DropView { view_name } => {
                    if reserved(view_name) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                AuthAction::Reindex { index_name } => {
                    if reserved(index_name) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                AuthAction::Function { function_name } => {
                    if function_name.eq_ignore_ascii_case("load_extension") {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
                _ => Authorization::Deny,
            }
        }));
        let state = capture.clone();
        let allowed = config.tables.clone();
        connection.preupdate_hook(Some(
            move |_, database: &str, table: &str, case: &PreUpdateCase| {
                if database != "main"
                    || table.starts_with("sqlite_")
                    || table.starts_with("__drasi_")
                    || (!allowed.is_empty() && !allowed.iter().any(|name| name == table))
                {
                    return;
                }
                match state.lock() {
                    Ok(mut capture) if capture.failure.is_none() => {
                        if let Err(error) = capture.capture(table, case) {
                            capture.failure = Some(error);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => log::error!("Native SQLite capture poisoned: {error}"),
                }
            },
        ));
        let state = capture.clone();
        connection.commit_hook(Some(move || {
            state
                .lock()
                .map_or(true, |capture| capture.failure.is_some())
        }));
        let state = capture.clone();
        let auth = authorization.clone();
        connection.progress_handler(
            1000,
            Some(move || {
                if auth.lock().is_ok_and(|state| state.internal) {
                    return false;
                }
                *shutdown.borrow()
                    || state.lock().map_or(true, |state| {
                        state
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline)
                    })
            }),
        );
        let database = Self {
            connection,
            config,
            capture,
            authorization,
            journal: None,
            fenced: false,
            schema_version: Cell::new(None),
        };
        database.refresh_tables()?;
        Ok(database)
    }

    fn internal<T>(&self, work: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .internal = true;
        let result = work(&self.connection);
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .internal = false;
        result
    }
    fn sql_internal(&self, sql: &str) -> Result<()> {
        self.internal(|connection| {
            connection.execute_batch(sql)?;
            Ok(())
        })
    }
    fn refresh_tables(&self) -> Result<()> {
        let version: i64 = self.internal(|connection| {
            Ok(connection.query_row("PRAGMA main.schema_version", [], |row| row.get(0))?)
        })?;
        if self.schema_version.get() == Some(version) {
            return Ok(());
        }
        let tables = self.internal(|connection| {
            let mut tables = BTreeMap::new();
            let mut listing = connection.prepare("SELECT name, type FROM pragma_table_list WHERE schema='main' AND lower(substr(name,1,7)) != 'sqlite_' AND lower(substr(name,1,8)) != '__drasi_' ORDER BY name")?;
            let mut rows = listing.query([])?;
            let mut bytes = 0usize;
            while let Some(row) = rows.next()? {
                let name: String = row.get(0)?;
                if !self.config.tables.is_empty() && !self.config.tables.contains(&name) { continue; }
                let kind: String = row.get(1)?;
                if kind == "view" { continue; }
                anyhow::ensure!(kind == "table", "native SQLite capture requires ordinary tables");
                let mut foreign_keys = connection.prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1)")?;
                let mut targets = foreign_keys.query([&name])?;
                while let Some(target) = targets.next()? {
                    anyhow::ensure!(!target.get::<_, String>(0)?.to_ascii_lowercase().starts_with("__drasi_"),
                        "native SQLite application tables cannot reference journal tables");
                }
                let mut info = connection.prepare("SELECT name, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid")?;
                let mut columns = info.query([&name])?;
                let mut table = Table { columns: Vec::new(), keys: Vec::new() };
                while let Some(column) = columns.next()? {
                    let column_name: String = column.get(0)?;
                    let hidden: i64 = column.get(2)?;
                    anyhow::ensure!(hidden == 0, "native SQLite capture does not support generated/hidden columns");
                    bytes = bytes.checked_add(name.len() + column_name.len()).context("SQLite schema byte overflow")?;
                    anyhow::ensure!(bytes <= self.config.transactions.max_bytes.get(), "SQLite schema exceeds byte limit");
                    if column.get::<_, i64>(1)? > 0 { table.keys.push(column_name.clone()); }
                    table.columns.push(column_name);
                }
                anyhow::ensure!(tables.len() < self.config.transactions.max_changes.get(), "SQLite schema table count exceeds limit");
                tables.insert(name, table);
            }
            Ok(tables)
        })?;
        self.capture
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
            .tables = tables;
        self.schema_version.set(Some(version));
        Ok(())
    }
    fn begin(&self) -> Result<Instant> {
        let deadline = Instant::now()
            .checked_add(self.config.transactions.duration())
            .context("SQLite transaction deadline overflow")?;
        {
            let mut capture = self
                .capture
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?;
            capture.clear();
            capture.timestamp = Utc::now();
            capture.deadline = Some(deadline);
        }
        self.sql_internal("BEGIN IMMEDIATE")?;
        self.refresh_tables()?;
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .scoped = true;
        Ok(deadline)
    }
    fn schema(&self) -> Result<String> {
        self.internal(|connection| {
            let mut statement = connection.prepare(
                "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE lower(substr(name,1,7))!='sqlite_' AND lower(substr(name,1,8))!='__drasi_' ORDER BY type,name"
            )?;
            let mut rows = statement.query([])?;
            let mut entries = Vec::new();
            let mut bytes = 0usize;
            while let Some(row) = rows.next()? {
                let table: String = row.get(2)?;
                if !self.config.tables.is_empty() && !self.config.tables.contains(&table) { continue; }
                let kind: String = row.get(0)?;
                let name: String = row.get(1)?;
                let sql: String = row.get(3)?;
                bytes = bytes.checked_add(kind.len() + name.len() + table.len() + sql.len()).context("SQLite schema size overflow")?;
                anyhow::ensure!(bytes <= self.config.transactions.max_bytes.get(), "native SQLite schema exceeds configured bound");
                entries.push((kind, name, table, sql));
            }
            Ok(serde_json::to_string(&entries)?)
        })
    }
    fn rollback(&self) -> Result<()> {
        if !self.connection.is_autocommit() {
            self.sql_internal("ROLLBACK")?;
        }
        anyhow::ensure!(
            self.connection.is_autocommit(),
            "SQLite rollback did not restore autocommit"
        );
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .scoped = false;
        self.capture
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
            .clear();
        self.refresh_tables()
    }
    fn check_capture(&self) -> Result<()> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?;
        if let Some(error) = capture.failure.take() {
            return Err(error);
        }
        anyhow::ensure!(
            capture
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline),
            "SQLite transaction deadline exceeded"
        );
        Ok(())
    }
    fn execute(&self, sql: &str, params: &[SqliteParam], script: bool) -> Result<usize> {
        let mut batch = Batch::new(&self.connection, sql);
        let mut count = 0usize;
        let mut statements = 0usize;
        loop {
            self.authorization
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
                .savepoint = None;
            let Some(mut statement) = batch.next()? else {
                break;
            };
            statements += 1;
            anyhow::ensure!(
                script || statements == 1,
                "execute accepts exactly one SQLite statement"
            );
            let values = values(params);
            let result = statement.execute(rusqlite::params_from_iter(values.iter()));
            self.check_capture()?;
            count = count
                .checked_add(result?)
                .context("SQLite affected-row count overflow")?;
            drop(statement);
            let savepoint = self
                .authorization
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
                .savepoint
                .take();
            if let Some(command) = savepoint {
                self.capture
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
                    .savepoint(command)?;
            }
            self.refresh_tables()?;
        }
        anyhow::ensure!(statements > 0, "SQLite statement is empty");
        Ok(count)
    }
    fn query(&self, sql: &str, params: &[SqliteParam]) -> Result<Vec<SqliteRow>> {
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .savepoint = None;
        let mut batch = Batch::new(&self.connection, sql);
        let mut statement = batch.next()?.context("SQLite query is empty")?;
        anyhow::ensure!(
            self.authorization
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
                .savepoint
                .is_none(),
            "SQLite query API cannot change savepoints"
        );
        anyhow::ensure!(
            statement.readonly(),
            "SQLite query API accepts only read-only statements"
        );
        anyhow::ensure!(
            batch.next()?.is_none(),
            "SQLite query API accepts exactly one statement"
        );
        let deadline = self
            .capture
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
            .deadline;
        let result = read_rows(&mut statement, params, self.config.transactions, deadline)?;
        self.check_capture()?;
        Ok(result)
    }
    fn committed(&mut self, stream: &StreamId, sequence: u64) -> Result<Option<OutputEnvelope>> {
        self.check_capture()?;
        let (source, changes, timestamp) = {
            let mut capture = self
                .capture
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?;
            (
                capture.source.clone(),
                std::mem::take(&mut capture.changes),
                capture.timestamp,
            )
        };
        let payload = if changes.is_empty() {
            None
        } else {
            Some(match self.config.output {
                SqliteOutput::Changes => {
                    let envelope = GraphChangeCodec::encode_changes(
                        &changes,
                        stream.clone(),
                        sequence,
                        Some(timestamp),
                    )?;
                    anyhow::ensure!(
                        bincode::serialized_size(&changes)?
                            <= self.config.transactions.max_bytes.get() as u64,
                        "SQLite output byte limit exceeded"
                    );
                    Pending::Changes(envelope)
                }
                SqliteOutput::Transactions => {
                    let mut builder =
                        SourceTransactionBuilder::new(source.as_str(), self.config.transactions)?;
                    for change in changes {
                        builder.push(change)?;
                    }
                    let identity = match &self.journal {
                        Some(journal) => journal.identity(
                            journal
                                .head
                                .checked_add(1)
                                .context("SQLite journal sequence exhausted")?,
                        )?,
                        None => Bytes::copy_from_slice(&sequence.to_be_bytes()),
                    };
                    Pending::Transaction(builder.prepare(identity.clone(), identity)?)
                }
            })
        };
        let appended = match (&self.journal, &payload) {
            (Some(journal), Some(Pending::Transaction(transaction))) => {
                let bytes = transaction.encode()?;
                Some(self.internal(|connection| journal.append(connection, &bytes, timestamp))?)
            }
            _ => None,
        };
        let schema = if let Some(journal) = &self.journal {
            let schema = self.schema()?;
            if schema != journal.schema {
                self.internal(|connection| {
                    anyhow::ensure!(connection.execute(
                        "UPDATE __drasi_native_state_v1 SET source_schema=?1 WHERE singleton=1 AND source_schema=?2",
                        rusqlite::params![schema, journal.schema],
                    )? == 1, "native SQLite stored schema changed unexpectedly");
                    Ok(())
                })?;
            }
            Some(schema)
        } else {
            None
        };
        self.check_capture()?;
        if let Err(error) = self.sql_internal("COMMIT") {
            self.fenced = self.connection.is_autocommit();
            return Err(error);
        }
        anyhow::ensure!(
            self.connection.is_autocommit(),
            "SQLite commit did not complete"
        );
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .scoped = false;
        self.capture
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
            .clear();
        if let Some(journal) = &mut self.journal {
            if let Some((head, bytes)) = appended {
                journal.head = head;
                journal.bytes = bytes;
            }
            journal.schema = schema.context("native SQLite committed schema missing")?;
            return Ok(None);
        }
        payload
            .map(|payload| {
                Ok(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: match payload {
                        Pending::Changes(envelope) => envelope,
                        Pending::Transaction(transaction) => transaction
                            .confirm_committed()
                            .into_envelope(stream.clone(), sequence, timestamp)?,
                    },
                })
            })
            .transpose()
    }

    fn feedback(&mut self, snapshot: &SourceProgressSnapshot) -> Result<()> {
        let Some(journal) = self.journal.as_mut() else {
            return Ok(());
        };
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .internal = true;
        let result = journal.feedback(&self.connection, snapshot);
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .internal = false;
        result
    }

    fn replay(
        &mut self,
        stream: &StreamId,
        sequence: &AtomicU64,
    ) -> Result<Option<OutputEnvelope>> {
        let Some(journal) = self.journal.as_mut() else {
            return Ok(None);
        };
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .internal = true;
        let result = journal.next(&self.connection, self.config.transactions);
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .internal = false;
        let Some((logical, timestamp, transaction)) = result? else {
            return Ok(None);
        };
        let transport = next_sequence(sequence)?;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: transaction.into_replay_envelope(
                stream.clone(),
                logical,
                transport,
                timestamp,
            )?,
        }))
    }
}

enum Pending {
    Changes(ChangeEnvelope),
    Transaction(PreparedSourceTransaction),
}

fn values(params: &[SqliteParam]) -> Vec<Value> {
    params
        .iter()
        .map(|param| match param {
            SqliteParam::Null => Value::Null,
            SqliteParam::Integer(value) => Value::Integer(*value),
            SqliteParam::Bool(value) => Value::Integer(i64::from(*value)),
            SqliteParam::Real(value) => Value::Real(*value),
            SqliteParam::Text(value) => Value::Text(value.clone()),
        })
        .collect()
}

fn read_rows(
    statement: &mut Statement<'_>,
    params: &[SqliteParam],
    limits: SourceTransactionLimits,
    deadline: Option<Instant>,
) -> Result<Vec<SqliteRow>> {
    let names: Vec<_> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    anyhow::ensure!(
        names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == names.len(),
        "duplicate SQLite query column names require aliases"
    );
    let values = values(params);
    let mut rows = statement.query(rusqlite::params_from_iter(values.iter()))?;
    let mut result = Vec::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        anyhow::ensure!(
            deadline.is_none_or(|deadline| Instant::now() < deadline),
            "SQLite query deadline exceeded"
        );
        anyhow::ensure!(
            result.len() < limits.max_changes.get(),
            "SQLite query row limit exceeded"
        );
        let mut item = SqliteRow::new();
        for (index, name) in names.iter().enumerate() {
            item.insert(name.clone(), json(row.get_ref(index)?)?);
        }
        bytes = bytes
            .checked_add(serde_json::to_vec(&item)?.len() as u64)
            .context("SQLite query byte count overflow")?;
        anyhow::ensure!(
            bytes <= limits.max_bytes.get() as u64,
            "SQLite query byte limit exceeded"
        );
        result.push(item);
    }
    Ok(result)
}

fn respond<T>(response: oneshot::Sender<Result<T>>, result: Result<T>) -> Result<()> {
    match result {
        Ok(value) => {
            let _ = response.send(Ok(value));
            Ok(())
        }
        Err(error) => {
            let failure = Failure(Arc::new(error));
            let _ = response.send(Err(failure.clone().into()));
            Err(failure.into())
        }
    }
}

fn scoped(
    database: &Database,
    commands: &mut mpsc::Receiver<TransactionCommand>,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Instant,
) -> Result<bool> {
    let runtime = tokio::runtime::Handle::current();
    loop {
        let command = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => anyhow::bail!("native SQLite transaction stopped"),
                _ = tokio::time::sleep_until(deadline) => anyhow::bail!("native SQLite transaction deadline exceeded"),
                command = commands.recv() => command.context("native SQLite transaction abandoned"),
            }
        })?;
        anyhow::ensure!(
            !database.connection.is_autocommit(),
            "native SQLite transaction was rolled back"
        );
        match command {
            TransactionCommand::Execute {
                sql,
                params,
                batch,
                response,
            } => respond(response, database.execute(&sql, &params, batch))?,
            TransactionCommand::Query {
                sql,
                params,
                response,
            } => respond(response, database.query(&sql, &params))?,
            TransactionCommand::Finish { commit } => return Ok(commit),
            TransactionCommand::Reject { error, response } => respond(response, Err(error))?,
        }
    }
}

pub(super) fn snapshot(
    mut work: super::snapshot::SnapshotWork,
) -> Result<Option<super::snapshot::Handover>> {
    let database = Database::open(work.config, work.source.clone(), work.shutdown.clone())?;
    if *work.shutdown.borrow() {
        return Ok(None);
    }
    let schema = database.schema()?;
    let journal = database.internal(|connection| {
        super::journal::Journal::open(
            connection,
            &database.config,
            work.source.clone(),
            &work.stream,
            (&work.owner, &work.progress),
            schema,
            Some(work.handover.epoch),
        )
    })?;
    anyhow::ensure!(
        journal.head == 0 && journal.retired == 0,
        "native SQLite cannot replace an initialized source with a fresh snapshot"
    );
    let deadline = database.begin()?;
    let (tables, timestamp) = {
        let capture = database
            .capture
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?;
        (
            capture.tables.clone(),
            u64::try_from(capture.timestamp.timestamp_millis())?,
        )
    };
    let runtime = tokio::runtime::Handle::current();
    let mut ordinal = 0u64;
    for (name, table) in tables {
        let mut statement = database
            .connection
            .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))?;
        let count = i32::try_from(statement.column_count())?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if *work.shutdown.borrow() {
                drop(rows);
                drop(statement);
                database.rollback()?;
                return Ok(None);
            }
            database.check_capture()?;
            let element = element(
                work.source.as_str(),
                &name,
                &table,
                timestamp,
                count,
                |index| row.get_ref(index as usize),
            )?;
            let change = SourceChange::Insert { element };
            anyhow::ensure!(
                bincode::serialized_size(&change)?
                    <= database.config.transactions.max_bytes.get() as u64,
                "native SQLite snapshot row exceeds byte limit"
            );
            ordinal = ordinal
                .checked_add(1)
                .context("SQLite snapshot ordinal exhausted")?;
            let change =
                GraphChangeCodec::encode_change(change, work.stream.clone(), ordinal, None)?;
            let sent = runtime.block_on(async {
                tokio::time::timeout_at(deadline, async {
                    tokio::select! {
                        biased;
                        _ = work.shutdown.wait_for(|stopped| *stopped) => Ok(false),
                        result = work.outputs.send(change) => result.map(|_| true).context("SQLite snapshot receiver closed"),
                    }
                }).await.context("native SQLite snapshot deadline exceeded")?
            })?;
            if !sent {
                drop(rows);
                drop(statement);
                database.rollback()?;
                return Ok(None);
            }
        }
    }
    database.check_capture()?;
    database.rollback()?;
    if *work.shutdown.borrow() {
        return Ok(None);
    }
    work.handover.completed = true;
    Ok(Some(work.handover))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    config: SqliteConfig,
    source: ComponentId,
    stream: StreamId,
    sequence: Arc<AtomicU64>,
    mut commands: mpsc::Receiver<Command>,
    outputs: mpsc::Sender<OutputEnvelope>,
    mut shutdown: watch::Receiver<bool>,
    ready: oneshot::Sender<()>,
    progress: Option<(
        SourceProgressReader,
        Arc<SourceProgressSnapshot>,
        Option<uuid::Uuid>,
    )>,
) -> Result<()> {
    let mut updates = progress
        .as_ref()
        .map(|(owner, _, _)| owner.subscribe())
        .transpose()?;
    let mut database = Database::open(config, source.clone(), shutdown.clone())?;
    if let Some((owner, snapshot, bootstrap)) = progress {
        let schema = database.schema()?;
        database.journal = Some(database.internal(|connection| {
            super::journal::Journal::open(
                connection,
                &database.config,
                source,
                &stream,
                (&owner, &snapshot),
                schema,
                bootstrap,
            )
        })?);
    }
    let runtime = tokio::runtime::Handle::current();
    let _ = ready.send(());
    let mut pending = None;
    loop {
        if let Some(progress) = &mut updates {
            database.feedback(progress.snapshot()?.as_ref())?;
        }
        if pending.is_none() {
            pending = database.replay(&stream, &sequence)?;
        }
        let wake = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => Wake::Stop,
                changed = async {
                    match &mut updates {
                        Some(progress) => progress.changed().await.context("native SQLite consumer progress closed"),
                        None => std::future::pending().await,
                    }
                } => Wake::Progress(changed),
                permit = outputs.reserve(), if pending.is_some() => Wake::Output(permit),
                command = commands.recv(), if pending.is_none() => Wake::Command(command),
            }
        });
        let command = match wake {
            Wake::Stop | Wake::Command(None) => {
                if pending.is_some() && database.journal.is_none() {
                    log::warn!("Stopping volatile native SQLite output; committed changes are not replayable");
                }
                return database.rollback();
            }
            Wake::Progress(result) => {
                result?;
                continue;
            }
            Wake::Output(result) => {
                result.context("native SQLite output closed")?.send(
                    pending
                        .take()
                        .context("native SQLite pending output missing")?,
                );
                continue;
            }
            Wake::Command(Some(command)) => command,
        };
        match command {
            Command::Query {
                sql,
                params,
                response,
            } => {
                {
                    let mut capture = database
                        .capture
                        .lock()
                        .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?;
                    capture.deadline =
                        Instant::now().checked_add(database.config.transactions.duration());
                }
                let result = database.query(&sql, &params);
                database
                    .capture
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
                    .deadline = None;
                let _ = response.send(result);
            }
            Command::Transaction {
                mut commands,
                ready,
                completed,
            } => {
                let deadline = match database.begin() {
                    Ok(deadline) => deadline,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        database.rollback()?;
                        continue;
                    }
                };
                let mut result = if ready.send(Ok(())).is_ok() {
                    scoped(&database, &mut commands, &mut shutdown, deadline)
                } else {
                    Ok(false)
                };
                commands.close();
                let mut output = None;
                if matches!(result, Ok(true)) {
                    result = (|| {
                        let sequence = if database.journal.is_some() {
                            0
                        } else {
                            next_sequence(&sequence)?
                        };
                        output = database.committed(&stream, sequence)?;
                        Ok(true)
                    })();
                }
                if !matches!(result, Ok(true)) {
                    if database.fenced {
                        let failure = Failure(Arc::new(
                            result
                                .err()
                                .context("native SQLite commit outcome is uncertain")?,
                        ));
                        let _ = completed.send(Err(failure.clone().into()));
                        return Err(failure.into());
                    }
                    if let Err(cleanup) = database.rollback() {
                        let error = match result {
                            Err(error) => drasi_lib::error::OperationFailures::new(
                                "native SQLite rollback failed",
                                vec![error, cleanup],
                            )
                            .into(),
                            Ok(_) => cleanup,
                        };
                        let failure = Failure(Arc::new(error));
                        let _ = completed.send(Err(failure.clone().into()));
                        return Err(failure.into());
                    }
                }
                if let Err(Err(error)) = completed.send(result.map(|_| ())) {
                    log::warn!("Native SQLite transaction failed after its caller left: {error:#}");
                }
                pending = output;
            }
        }
    }
}

enum Wake<'a> {
    Stop,
    Progress(Result<()>),
    Output(std::result::Result<mpsc::Permit<'a, OutputEnvelope>, mpsc::error::SendError<()>>),
    Command(Option<Command>),
}

fn next_sequence(sequence: &AtomicU64) -> Result<u64> {
    Ok(sequence
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| anyhow::anyhow!("native SQLite sequence exhausted"))?
        + 1)
}
