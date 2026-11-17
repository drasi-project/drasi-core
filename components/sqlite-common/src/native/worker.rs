// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    error::{SqlitePaused, SqliteStopping},
    handle::{Command, TransactionCommand},
    journal::{GroupHandover, Handover},
    progress::{Observed, Reader as ProgressReader},
    SqliteConfig, SqliteConnectionOptions, SqliteOutput, SqliteParam, SqliteRow,
};
use crate::{row_element as element, value_to_json as json, TableSchema as Table};
use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentId, ConsumerProgressView, GraphChangeCodec, OutputEnvelope, PortId,
    PreparedSourceTransaction, SourceProgressSnapshot, SourceProgressView,
    SourceTransactionBuilder, SourceTransactionLimits, StreamId,
};
use drasi_core_types::models::SourceChange;
use rusqlite::{
    hooks::{AuthAction, AuthContext, Authorization, PreUpdateCase, TransactionOperation},
    limits::Limit,
    types::Value,
    Batch, Connection, Statement,
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

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
                    Some(row.get_new_row_id()),
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
                    Some(row.get_old_row_id()),
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
                    Some(before.get_old_row_id()),
                    before.get_column_count(),
                    |i| before.get_old_column_value(i),
                )?);
                new = Some(element(
                    source,
                    name,
                    &table,
                    time,
                    Some(after.get_new_row_id()),
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
                    self.push(SourceChange::update(after))?;
                }
            }
            (None, None) => anyhow::bail!("empty SQLite row change"),
        }
        Ok(())
    }
}

struct Database {
    connection: Connection,
    config: SqliteConfig,
    capture: Arc<Mutex<Capture>>,
    authorization: Arc<Mutex<AuthorizationState>>,
    journal: Option<super::journal::Journal>,
    fenced: bool,
    schema_version: Cell<Option<i64>>,
    /// The capture identity element IDs are bound to; the journal records it.
    identity: super::identity::CaptureIdentity,
    /// The reserved objects present when the first transaction began, after
    /// any journal was opened.
    reserved: std::cell::RefCell<Option<std::collections::BTreeSet<String>>>,
    /// The schema version when the current transaction began.
    begun_version: Cell<Option<i64>>,
}

/// Runs `initSql` with a guard that keeps it away from the source's own
/// journal objects, attached databases and extensions. Everything else,
/// including DDL and pragmas, is allowed: the regular authorizer is installed
/// afterwards.
fn run_init_sql(connection: &Connection, sql: &str) -> Result<()> {
    connection.authorizer(Some(|context: AuthContext<'_>| {
        let reserved = |name: Option<&str>| {
            name.is_some_and(|name| name.to_ascii_lowercase().starts_with("__drasi_"))
        };
        match context.action {
            AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
            AuthAction::Function { function_name }
                if function_name.eq_ignore_ascii_case("load_extension") =>
            {
                Authorization::Deny
            }
            _ => match init_object(&context.action) {
                Some(name) if reserved(Some(name)) => Authorization::Deny,
                _ => Authorization::Allow,
            },
        }
    }));
    let result = connection
        .execute_batch(sql)
        .context("native SQLite initSql failed");
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    result?;
    anyhow::ensure!(
        connection.is_autocommit(),
        "native SQLite initSql left a transaction open"
    );
    Ok(())
}

/// The object an authorizer action names, for the `initSql` reserved-name guard.
fn init_object<'a>(action: &AuthAction<'a>) -> Option<&'a str> {
    match *action {
        AuthAction::Read { table_name, .. }
        | AuthAction::Insert { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Delete { table_name }
        | AuthAction::CreateTable { table_name }
        | AuthAction::DropTable { table_name }
        | AuthAction::AlterTable { table_name, .. }
        | AuthAction::Analyze { table_name }
        | AuthAction::CreateTempTable { table_name }
        | AuthAction::DropTempTable { table_name } => Some(table_name),
        AuthAction::CreateIndex { index_name, .. }
        | AuthAction::DropIndex { index_name, .. }
        | AuthAction::CreateTempIndex { index_name, .. }
        | AuthAction::DropTempIndex { index_name, .. }
        | AuthAction::Reindex { index_name } => Some(index_name),
        AuthAction::CreateTrigger { trigger_name, .. }
        | AuthAction::DropTrigger { trigger_name, .. }
        | AuthAction::CreateTempTrigger { trigger_name, .. }
        | AuthAction::DropTempTrigger { trigger_name, .. } => Some(trigger_name),
        AuthAction::CreateView { view_name }
        | AuthAction::DropView { view_name }
        | AuthAction::CreateTempView { view_name }
        | AuthAction::DropTempView { view_name } => Some(view_name),
        _ => None,
    }
}

impl Database {
    fn open(
        config: SqliteConfig,
        options: &SqliteConnectionOptions,
        source: ComponentId,
        interrupt: watch::Receiver<bool>,
    ) -> Result<Self> {
        options.validate(&config)?;
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
            if let Some(sql) = &options.init_sql {
                run_init_sql(&connection, sql)?;
            }
        } else {
            if options.wal {
                let mode: String =
                    connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
                anyhow::ensure!(
                    mode.eq_ignore_ascii_case("wal"),
                    "native SQLite could not enable WAL mode (journal_mode is {mode})"
                );
            }
            if let Some(sql) = &options.init_sql {
                run_init_sql(&connection, sql)?;
            }
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
                // Schema changes are checked against the capture identity at
                // commit: only one that would orphan element IDs is refused.
                | AuthAction::AlterTable { table_name, .. }
                | AuthAction::DropTable { table_name }
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
                *interrupt.borrow()
                    || state.lock().map_or(true, |state| {
                        state
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline)
                    })
            }),
        );
        let mut database = Self {
            connection,
            config,
            capture,
            authorization,
            journal: None,
            fenced: false,
            schema_version: Cell::new(None),
            identity: super::identity::CaptureIdentity::new(&BTreeMap::new()),
            reserved: std::cell::RefCell::new(None),
            begun_version: Cell::new(None),
        };
        database.identity = database.current_identity()?;
        Ok(database)
    }

    /// The names of the source's reserved (`__drasi_`) schema objects.
    fn reserved_objects(&self) -> Result<std::collections::BTreeSet<String>> {
        self.internal(|connection| {
            let mut statement = connection.prepare(
                "SELECT name FROM sqlite_schema WHERE lower(substr(name,1,8))='__drasi_'",
            )?;
            let names = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(names)
        })
    }

    /// The capture identity of the current schema.
    fn current_identity(&self) -> Result<super::identity::CaptureIdentity> {
        self.refresh_tables()?;
        Ok(super::identity::CaptureIdentity::new(
            &self
                .capture
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
                .tables,
        ))
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
            crate::read_table_schemas(
                connection,
                &self.config.tables,
                &self.config.table_keys,
                self.config.transactions.max_changes.get(),
                self.config.transactions.max_bytes.get(),
            )
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
        if self.reserved.borrow().is_none() {
            *self.reserved.borrow_mut() = Some(self.reserved_objects()?);
        }
        self.begun_version.set(self.schema_version.get());
        self.authorization
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite authorizer poisoned"))?
            .scoped = true;
        Ok(deadline)
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
                        SourceTransactionBuilder::new(&source, self.config.transactions)?;
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
        // DDL in this transaction may only extend the capture identity; the
        // journal records the extension in the same transaction.
        let identity = self.current_identity()?;
        if self.schema_version.get() != self.begun_version.get() {
            // The authorizer cannot see a rename's target name; a renamed
            // table must not take a reserved journal name.
            anyhow::ensure!(
                self.reserved.borrow().as_ref() == Some(&self.reserved_objects()?),
                "native SQLite DDL cannot create or rename __drasi_ objects"
            );
        }
        let schema = if identity != self.identity {
            identity.check_extends(&self.identity)?;
            let encoded = identity.encode()?;
            if let Some(journal) = &self.journal {
                self.internal(|connection| {
                    anyhow::ensure!(connection.execute(
                        "UPDATE __drasi_native_state_v1 SET source_schema=?1 WHERE singleton=1 AND source_schema=?2",
                        rusqlite::params![encoded, journal.schema],
                    )? == 1, "native SQLite stored capture identity changed unexpectedly");
                    Ok(())
                })?;
            }
            Some(encoded)
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
            if let Some(schema) = schema {
                journal.schema = schema;
            }
            self.identity = identity;
            return Ok(None);
        }
        self.identity = identity;
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

    fn feedback(&mut self, snapshot: &Observed) -> Result<()> {
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

/// Serves one transaction scope. Abort rolls it back at once. Stop rolls it
/// back too unless the worker drains open transactions, in which case it may
/// finish within its own deadline.
fn scoped(
    database: &Database,
    commands: &mut mpsc::Receiver<TransactionCommand>,
    control: &mut WorkerControl,
    deadline: Instant,
) -> Result<bool> {
    let runtime = tokio::runtime::Handle::current();
    let drain = control.drain_transactions;
    let (abort, stop) = (&mut control.abort, &mut control.stop);
    loop {
        let command = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = abort.wait_for(|aborted| *aborted) => anyhow::bail!("native SQLite transaction aborted by Stop"),
                _ = stop.wait_for(|stopped| *stopped), if !drain => anyhow::bail!("native SQLite transaction stopped"),
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

pub struct SnapshotWork {
    pub source: ComponentId,
    pub stream: StreamId,
    pub config: SqliteConfig,
    pub options: SqliteConnectionOptions,
    pub owner: SourceProgressView,
    pub progress: Arc<SourceProgressSnapshot>,
    pub handover: Handover,
    pub outputs: mpsc::Sender<ChangeEnvelope>,
    pub shutdown: watch::Receiver<bool>,
}

pub fn snapshot(mut work: SnapshotWork) -> Result<Option<Handover>> {
    let database = Database::open(
        work.config,
        &work.options,
        work.source.clone(),
        work.shutdown.clone(),
    )?;
    if *work.shutdown.borrow() {
        return Ok(None);
    }
    let schema = database.identity.encode()?;
    let journal = database.internal(|connection| {
        super::journal::Journal::open(
            connection,
            &database.config,
            work.source.clone(),
            &work.stream,
            (
                &ProgressReader::Single(work.owner.clone()),
                &Observed::Single(work.progress.clone()),
            ),
            schema,
            Some(work.handover.epoch),
        )
    })?;
    anyhow::ensure!(
        journal.head == 0 && journal.retired == 0,
        "native SQLite cannot replace an initialized source with a fresh snapshot"
    );
    if !snapshot_rows(
        &database,
        &work.source,
        &work.stream,
        &work.outputs,
        &mut work.shutdown,
        None,
    )? {
        return Ok(None);
    }
    work.handover.completed = true;
    Ok(Some(work.handover))
}

fn validate_snapshot_owner(owner: &SourceProgressSnapshot, generation: u64) -> Result<()> {
    anyhow::ensure!(
        owner.recovered && owner.failure.is_none() && owner.reset_generation == generation,
        "native SQLite snapshot consumer is failed, reset or not recovered"
    );
    Ok(())
}

fn snapshot_rows(
    database: &Database,
    source: &ComponentId,
    stream: &StreamId,
    outputs: &mpsc::Sender<ChangeEnvelope>,
    shutdown: &mut watch::Receiver<bool>,
    progress: Option<(&SourceProgressView, u64)>,
) -> Result<bool> {
    let mut progress = progress
        .map(|(owner, generation)| Ok::<_, anyhow::Error>((owner.subscribe()?, generation)))
        .transpose()?;
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
        let outcome = crate::read_table(
            &database.connection,
            source.as_str(),
            &name,
            &table,
            timestamp,
            |element| {
                if *shutdown.borrow() {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                database.check_capture()?;
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
                    GraphChangeCodec::encode_change(change, stream.clone(), ordinal, None)?;
                let sent =
                    runtime.block_on(async {
                        tokio::time::timeout_at(deadline, async {
                        loop {
                            if let Some((progress, generation)) = &mut progress {
                                validate_snapshot_owner(progress.snapshot()?.as_ref(), *generation)?;
                            }
                            tokio::select! {
                                biased;
                                _ = shutdown.wait_for(|stopped| *stopped) => return Ok::<_, anyhow::Error>(false),
                                result = async {
                                    match &mut progress {
                                        Some((progress, _)) => progress.changed().await,
                                        None => std::future::pending().await,
                                    }
                                } => { result?; },
                                permit = outputs.reserve() => {
                                    permit.context("SQLite snapshot receiver closed")?.send(change);
                                    return Ok(true);
                                },
                            }
                        }
                    }).await.context("native SQLite snapshot deadline exceeded")?
                    })?;
                if !sent {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )?;
        if outcome.is_break() {
            database.rollback()?;
            return Ok(false);
        }
    }
    database.check_capture()?;
    database.rollback()?;
    if let Some((progress, generation)) = &mut progress {
        validate_snapshot_owner(progress.snapshot()?.as_ref(), *generation)?;
    }
    Ok(!*shutdown.borrow())
}

pub struct GroupSnapshotWork {
    pub source: ComponentId,
    pub stream: StreamId,
    pub config: SqliteConfig,
    pub options: SqliteConnectionOptions,
    pub consumers: ConsumerProgressView,
    pub member: SourceProgressView,
    pub generation: u64,
    pub previous: Option<GroupHandover>,
    pub prepared: oneshot::Sender<GroupHandover>,
    pub proceed: oneshot::Receiver<()>,
    pub outputs: mpsc::Sender<ChangeEnvelope>,
    pub shutdown: watch::Receiver<bool>,
}

pub fn group_snapshot(mut work: GroupSnapshotWork) -> Result<Option<GroupHandover>> {
    let database = Database::open(
        work.config,
        &work.options,
        work.source.clone(),
        work.shutdown.clone(),
    )?;
    if *work.shutdown.borrow() {
        return Ok(None);
    }
    validate_snapshot_owner(work.member.snapshot()?.as_ref(), work.generation)?;
    let binding = super::journal::group_binding(
        &database.config,
        &work.source,
        &work.stream,
        &work.consumers,
        true,
    )?;
    let mut handover = database.internal(|connection| {
        super::journal::plan_group_snapshot(
            connection,
            &binding,
            work.member.component_id(),
            work.previous.as_ref(),
        )
    })?;
    if work.prepared.send(handover.clone()).is_err() {
        return Ok(None);
    }
    let proceed = tokio::runtime::Handle::current().block_on(async {
        tokio::select! {
            biased;
            _ = work.shutdown.wait_for(|stopped| *stopped) => false,
            result = &mut work.proceed => result.is_ok(),
        }
    });
    if !proceed {
        return Ok(None);
    }
    validate_snapshot_owner(work.member.snapshot()?.as_ref(), work.generation)?;
    let schema = database.identity.encode()?;
    let observed = Observed::Group(work.consumers.snapshot()?);
    let journal = database.internal(|connection| {
        let journal = super::journal::Journal::open_mode(
            connection,
            &database.config,
            work.source.clone(),
            &work.stream,
            (&ProgressReader::Group(work.consumers.clone()), &observed),
            schema,
            Some(handover.epoch),
            true,
        )?;
        anyhow::ensure!(
            journal.head == handover.sequence,
            "SQLite snapshot boundary changed while recording initialization"
        );
        super::journal::enroll_group_snapshot(connection, &handover)?;
        Ok(journal)
    })?;
    anyhow::ensure!(
        journal.epoch == handover.epoch,
        "SQLite snapshot journal epoch changed"
    );
    if !snapshot_rows(
        &database,
        &work.source,
        &work.stream,
        &work.outputs,
        &mut work.shutdown,
        Some((&work.member, work.generation)),
    )? {
        return Ok(None);
    }
    handover.completed = true;
    Ok(Some(handover))
}

/// Lifecycle signals for the live worker.
pub struct WorkerControl {
    /// `true` from the start of a Stop: keep serving in-flight requests, but
    /// never wait for output (undeliverable volatile output is dropped and
    /// counted; journal output stays in the journal).
    pub draining: watch::Receiver<bool>,
    /// `true` once in-flight requests have drained: refuse queued commands
    /// and exit.
    pub stop: watch::Receiver<bool>,
    /// `true` when Stop is an Abort: open work is rolled back at once.
    pub abort: watch::Receiver<bool>,
    /// `true` while the host asks the source to pause.
    pub pause: watch::Receiver<bool>,
    /// Set by the worker when it has parked at a transaction boundary.
    pub paused: Arc<watch::Sender<bool>>,
    /// Whether Stop lets an open transaction scope finish instead of rolling
    /// it back.
    pub drain_transactions: bool,
}

/// How the live worker ended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkerOutcome {
    /// Committed volatile transactions whose output could not be delivered
    /// before Stop. Journal modes never lose output: it stays in the journal.
    pub lost_volatile_transactions: usize,
}

/// Answers every queued command with [`SqliteStopping`] without executing it,
/// then closes the lane so later submissions fail immediately.
fn reject_queued(commands: &mut mpsc::Receiver<Command>) {
    commands.close();
    while let Ok(command) = commands.try_recv() {
        let stopping = || anyhow::Error::new(SqliteStopping);
        match command {
            Command::DescribeSchema { response } => {
                let _ = response.send(Err(stopping()));
            }
            Command::KeyColumns { response, .. } => {
                let _ = response.send(Err(stopping()));
            }
            Command::Query { response, .. } => {
                let _ = response.send(Err(stopping()));
            }
            Command::Transaction { ready, .. } => {
                let _ = ready.send(Err(stopping()));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    config: SqliteConfig,
    options: SqliteConnectionOptions,
    source: ComponentId,
    stream: StreamId,
    sequence: Arc<AtomicU64>,
    mut commands: mpsc::Receiver<Command>,
    outputs: mpsc::Sender<OutputEnvelope>,
    mut control: WorkerControl,
    ready: oneshot::Sender<()>,
    progress: Option<(ProgressReader, Observed, Option<uuid::Uuid>)>,
) -> Result<WorkerOutcome> {
    let mut updates = progress
        .as_ref()
        .map(|(owner, _, _)| owner.subscribe())
        .transpose()?;
    let mut database = Database::open(config, &options, source.clone(), control.abort.clone())?;
    if let Some((owner, snapshot, bootstrap)) = progress {
        let schema = database.identity.encode()?;
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
    let mut lost = 0usize;
    loop {
        if let Some(progress) = &mut updates {
            database.feedback(&progress.snapshot()?)?;
        }
        // The loop top is always a transaction boundary: acknowledge Pause
        // and Resume here, so a paused worker never holds an open write.
        let pausing = *control.pause.borrow_and_update();
        if *control.paused.borrow() != pausing {
            control.paused.send_replace(pausing);
        }
        let draining = *control.draining.borrow_and_update();
        if draining {
            // Never wait for output while stopping: deliver what fits now.
            if let Some(output) = pending.take() {
                match outputs.try_reserve() {
                    Ok(permit) => permit.send(output),
                    Err(_) if database.journal.is_none() => lost += 1,
                    Err(_) => {}
                }
            }
        } else if pending.is_none() && !pausing {
            pending = database.replay(&stream, &sequence)?;
        }
        let wake = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = control.stop.wait_for(|stopped| *stopped) => Wake::Stop,
                changed = control.pause.changed() => match changed {
                    Ok(()) => Wake::Pause,
                    Err(_) => Wake::Stop,
                },
                changed = control.draining.changed(), if !draining => match changed {
                    Ok(()) => Wake::Pause,
                    Err(_) => Wake::Stop,
                },
                changed = async {
                    match &mut updates {
                        Some(progress) => progress.changed().await.context("native SQLite consumer progress closed"),
                        None => std::future::pending().await,
                    }
                } => Wake::Progress(changed),
                permit = outputs.reserve(), if pending.is_some() && !pausing && !draining => Wake::Output(permit),
                // Writes wait while output is pending (one envelope of
                // buffered memory). A paused worker still answers reads and
                // refuses writes, and a draining worker never holds output,
                // so callers never wait on held output.
                command = commands.recv(), if pending.is_none() || pausing || draining => Wake::Command(command),
            }
        });
        let command = match wake {
            Wake::Stop | Wake::Command(None) => {
                reject_queued(&mut commands);
                if let Some(updates) = &mut updates {
                    let snapshot = updates.snapshot()?;
                    if matches!(&snapshot, Observed::Group(_)) && snapshot.ready() {
                        database.feedback(&snapshot)?;
                    }
                }
                let mut outcome = WorkerOutcome {
                    lost_volatile_transactions: lost,
                };
                if let Some(output) = pending.take() {
                    let delivered = !*control.abort.borrow()
                        && match outputs.try_reserve() {
                            Ok(permit) => {
                                permit.send(output);
                                true
                            }
                            Err(_) => false,
                        };
                    if !delivered && database.journal.is_none() {
                        outcome.lost_volatile_transactions += 1;
                    }
                }
                database.rollback()?;
                return Ok(outcome);
            }
            Wake::Pause => continue,
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
            Command::DescribeSchema { response } => {
                {
                    let mut capture = database
                        .capture
                        .lock()
                        .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?;
                    capture.deadline =
                        Instant::now().checked_add(database.config.transactions.duration());
                }
                let result = database.internal(|connection| {
                    crate::schema::describe_source(
                        connection,
                        &database.config.tables,
                        &database.config.table_keys,
                        database.config.transactions.max_changes.get(),
                        database.config.transactions.max_bytes.get(),
                    )
                });
                database
                    .capture
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?
                    .deadline = None;
                let _ = response.send(result);
            }
            Command::KeyColumns { table, response } => {
                let result = (|| -> Result<Vec<String>> {
                    let capture = database
                        .capture
                        .lock()
                        .map_err(|_| anyhow::anyhow!("SQLite capture poisoned"))?;
                    let table = capture
                        .tables
                        .get(&table)
                        .context("native SQLite table is not captured by this source")?;
                    Ok(match table.rowid_alias {
                        Some(alias) => vec![alias.into()],
                        None => table.keys.clone(),
                    })
                })();
                let _ = response.send(result);
            }
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
            Command::Transaction { ready, .. } if pausing => {
                let _ = ready.send(Err(anyhow::Error::new(SqlitePaused)));
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
                    scoped(&database, &mut commands, &mut control, deadline)
                } else {
                    Ok(false)
                };
                commands.close();
                let mut output = None;
                if matches!(result, Ok(true)) {
                    result = (|| {
                        if let (Some(updates), Some(journal)) = (&mut updates, &database.journal) {
                            if matches!(updates, super::progress::Updates::Group(_)) {
                                journal.position(&updates.snapshot()?)?;
                            }
                        }
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
                            Err(error) => drasi_computation_contracts::OperationFailures::new(
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
    Pause,
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
