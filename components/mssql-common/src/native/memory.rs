// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! An in-memory [`CdcConnector`] for database-free tests (feature
//! `test-support`). It models CDC change tables, capture instances, LSN
//! retention, the catalog fingerprint and transport faults with the same
//! query semantics as the Tiberius session, so paging, cursor, grouping,
//! reconnect, pause and stop behaviour can be tested without SQL Server.
//! It is not a SQL Server emulator: SQL text is never executed.

use super::{
    catalog::Table,
    session::{
        CaptureInstance, CdcConnector, CdcSession, DatabaseIdentity, Key, KeyRequest, RawColumn,
        RawRow, TableFacts,
    },
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, TimeZone};
use drasi_core_types::models::{ElementPropertyMap, ElementValue};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// One captured table.
#[derive(Clone, Debug)]
pub struct MemoryTable {
    pub capture: CaptureInstance,
    pub facts: TableFacts,
    pub min_lsn: [u8; 10],
    /// `(key, operation, commit time, properties)`.
    pub rows: Vec<(Key, i32, DateTime<FixedOffset>, ElementPropertyMap)>,
}

/// The shared database state.
#[derive(Debug)]
pub struct MemoryState {
    pub database: DatabaseIdentity,
    pub tables: Vec<MemoryTable>,
    pub max_lsn: [u8; 10],
    pub fingerprint: u64,
    pub time_zone: Option<String>,
    pub selectable: bool,
    /// Connection attempts that fail with a connection reset.
    pub failing_connects: usize,
    /// Queries that fail with a connection reset (on any session).
    pub failing_queries: usize,
    /// Fails every connection attempt with a login failure (permanent).
    pub login_denied: bool,
    /// While set, `next_keys` waits until released.
    pub blocked: bool,
    /// Key reads currently waiting on `blocked`.
    pub waiting: usize,
    /// Barrier marker tokens, when the barrier table exists.
    pub markers: Option<Vec<String>>,
    pub connects: usize,
    pub key_queries: usize,
    pub range_queries: usize,
    pub open_sessions: usize,
    pub closed_sessions: usize,
}

/// An in-memory CDC database.
#[derive(Clone)]
pub struct MemoryDatabase {
    state: Arc<Mutex<MemoryState>>,
    released: Arc<Notify>,
}

/// LSN `n` as a 10-byte big-endian value.
pub fn lsn(n: u64) -> [u8; 10] {
    let mut bytes = [0; 10];
    bytes[2..].copy_from_slice(&n.to_be_bytes());
    bytes
}

/// A UTC commit time `seconds` after the epoch.
pub fn utc(seconds: i64) -> DateTime<FixedOffset> {
    FixedOffset::east_opt(0)
        .expect("UTC offset")
        .timestamp_opt(seconds, 0)
        .single()
        .expect("valid timestamp")
}

fn reset() -> anyhow::Error {
    anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
}

impl Default for MemoryDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryDatabase {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MemoryState {
                database: DatabaseIdentity {
                    guid: "00000000-0000-0000-0000-00000000d8b1".into(),
                    name: "app".into(),
                },
                tables: Vec::new(),
                max_lsn: lsn(1),
                fingerprint: 1,
                time_zone: Some("UTC".into()),
                selectable: true,
                failing_connects: 0,
                failing_queries: 0,
                login_denied: false,
                blocked: false,
                waiting: 0,
                markers: None,
                connects: 0,
                key_queries: 0,
                range_queries: 0,
                open_sessions: 0,
                closed_sessions: 0,
            })),
            released: Arc::new(Notify::new()),
        }
    }

    /// Runs `change` on the state. Never call it from inside another lock.
    pub fn with<T>(&self, change: impl FnOnce(&mut MemoryState) -> T) -> T {
        let mut state = self.state.lock().expect("memory database lock");
        change(&mut state)
    }

    /// Adds `dbo.<name>` with columns `id int` (primary key) and
    /// `value int`, captured as `dbo_<name>`.
    pub fn add_table(&self, name: &str, object: i32) {
        let column = |name: &str| RawColumn {
            name: name.into(),
            type_name: "int".into(),
            precision: 10,
            scale: 0,
        };
        let columns = vec![column("id"), column("value")];
        self.with(|state| {
            state.tables.push(MemoryTable {
                capture: CaptureInstance {
                    schema: "dbo".into(),
                    table: name.into(),
                    object,
                    capture_object: object + 10_000,
                    capture: format!("dbo_{name}"),
                },
                facts: TableFacts {
                    columns: columns.clone(),
                    source_columns: columns,
                    primary_key: vec!["id".into()],
                },
                min_lsn: lsn(1),
                rows: Vec::new(),
            });
            state.fingerprint += 1;
        });
    }

    /// Commits one row change in `object` at LSN `at`, sequence `sequence`.
    pub fn change(&self, object: i32, at: u64, sequence: u64, operation: i32, id: i64, value: i64) {
        self.change_at(object, at, sequence, operation, id, value, utc(at as i64));
    }

    /// As [`Self::change`], with an explicit commit time.
    #[allow(clippy::too_many_arguments)]
    pub fn change_at(
        &self,
        object: i32,
        at: u64,
        sequence: u64,
        operation: i32,
        id: i64,
        value: i64,
        commit: DateTime<FixedOffset>,
    ) {
        let mut properties = ElementPropertyMap::new();
        properties.insert("id", ElementValue::Integer(id));
        properties.insert("value", ElementValue::Integer(value));
        self.with(|state| {
            let table = state
                .tables
                .iter_mut()
                .find(|table| table.capture.object == object)
                .expect("memory table");
            table.rows.push((
                Key {
                    lsn: lsn(at),
                    sequence: lsn(sequence),
                    table: object,
                },
                operation,
                commit,
                properties,
            ));
            if lsn(at) > state.max_lsn {
                state.max_lsn = lsn(at);
            }
        });
    }

    /// Runs the CDC cleanup job up to (excluding) LSN `low`.
    pub fn cleanup(&self, low: u64) {
        self.with(|state| {
            for table in &mut state.tables {
                table.rows.retain(|(key, ..)| key.lsn >= lsn(low));
                if table.min_lsn < lsn(low) {
                    table.min_lsn = lsn(low);
                }
            }
        });
    }

    /// Blocks key reads until [`Self::release`].
    pub fn block(&self) {
        self.with(|state| state.blocked = true);
    }

    /// Releases blocked key reads.
    pub fn release(&self) {
        self.with(|state| state.blocked = false);
        self.released.notify_waiters();
    }

    /// Waits until a key read is blocked.
    pub async fn wait_blocked(&self) {
        while self.with(|state| state.waiting) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    fn fault(&self) -> Result<()> {
        self.with(|state| {
            if state.failing_queries > 0 {
                state.failing_queries -= 1;
                Err(reset())
            } else {
                Ok(())
            }
        })
    }
}

#[async_trait]
impl CdcConnector for MemoryDatabase {
    async fn connect(&self) -> Result<Box<dyn CdcSession>> {
        self.with(|state| {
            state.connects += 1;
            if state.login_denied {
                return Err(anyhow::anyhow!("login failed (memory database)"));
            }
            if state.failing_connects > 0 {
                state.failing_connects -= 1;
                return Err(reset());
            }
            state.open_sessions += 1;
            Ok(())
        })?;
        Ok(Box::new(MemorySession {
            database: self.clone(),
        }))
    }
}

/// Counts a key read parked on `blocked`, including when it is dropped.
struct Waiting(MemoryDatabase);

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.with(|state| state.waiting -= 1);
    }
}

struct MemorySession {
    database: MemoryDatabase,
}

impl Drop for MemorySession {
    fn drop(&mut self) {
        self.database.with(|state| state.open_sessions -= 1);
    }
}

#[async_trait]
impl CdcSession for MemorySession {
    async fn database(&mut self) -> Result<DatabaseIdentity> {
        self.database.fault()?;
        Ok(self.database.with(|state| state.database.clone()))
    }
    async fn capture_instances(&mut self) -> Result<Vec<CaptureInstance>> {
        self.database.fault()?;
        Ok(self
            .database
            .with(|state| state.tables.iter().map(|t| t.capture.clone()).collect()))
    }
    async fn table_facts(
        &mut self,
        object: i32,
        _capture_object: Option<i32>,
    ) -> Result<TableFacts> {
        self.database.fault()?;
        self.database.with(|state| {
            state
                .tables
                .iter()
                .find(|t| t.capture.object == object)
                .map(|t| t.facts.clone())
                .context("memory table facts")
        })
    }
    async fn find_table(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<Option<(i32, String, String)>> {
        self.database.fault()?;
        Ok(self.database.with(|state| {
            state
                .tables
                .iter()
                .find(|t| {
                    t.capture.schema.eq_ignore_ascii_case(schema)
                        && t.capture.table.eq_ignore_ascii_case(table)
                })
                .map(|t| {
                    (
                        t.capture.object,
                        t.capture.schema.clone(),
                        t.capture.table.clone(),
                    )
                })
        }))
    }
    async fn can_select(&mut self, _object: &str) -> Result<bool> {
        self.database.fault()?;
        Ok(self.database.with(|state| state.selectable))
    }
    async fn catalog_fingerprint(&mut self) -> Result<String> {
        self.database.fault()?;
        Ok(self.database.with(|state| state.fingerprint.to_string()))
    }
    async fn time_zone(&mut self, configured: Option<&str>) -> Result<String> {
        self.database.fault()?;
        match configured {
            Some(zone) => Ok(zone.to_owned()),
            None => self
                .database
                .with(|state| state.time_zone.clone())
                .ok_or_else(|| super::error::unsupported("set serverTimeZone")),
        }
    }
    async fn max_lsn(&mut self) -> Result<[u8; 10]> {
        self.database.fault()?;
        Ok(self.database.with(|state| state.max_lsn))
    }
    async fn min_lsns(&mut self, captures: &[&str]) -> Result<Vec<[u8; 10]>> {
        self.database.fault()?;
        Ok(self.database.with(|state| {
            captures
                .iter()
                .map(|capture| {
                    state
                        .tables
                        .iter()
                        .find(|t| t.capture.capture == *capture)
                        .map_or([0; 10], |t| t.min_lsn)
                })
                .collect()
        }))
    }
    async fn next_keys(&mut self, request: KeyRequest<'_>) -> Result<Vec<Key>> {
        loop {
            let notified = self.database.released.notified();
            if !self.database.with(|state| state.blocked) {
                break;
            }
            self.database.with(|state| state.waiting += 1);
            let guard = Waiting(self.database.clone());
            notified.await;
            drop(guard);
        }
        self.database.fault()?;
        Ok(self.database.with(|state| {
            state.key_queries += 1;
            let mut merged = Vec::new();
            for (table, start) in request.tables.iter().zip(request.starts) {
                let Some(memory) = state
                    .tables
                    .iter()
                    .find(|t| t.capture.capture == table.capture)
                else {
                    continue;
                };
                let mut keys: Vec<Key> = memory
                    .rows
                    .iter()
                    .map(|(key, ..)| *key)
                    .filter(|key| key.lsn >= *start && key.lsn <= request.max)
                    .filter(|key| match request.after {
                        None => true,
                        Some(after) => match after.row {
                            None => key.lsn > after.lsn,
                            Some((sequence, object, _)) => {
                                key.lsn > after.lsn
                                    || (key.lsn == after.lsn
                                        && (key.sequence > sequence
                                            || (key.sequence == sequence
                                                && table.object >= object)))
                            }
                        },
                    })
                    .collect();
                keys.sort();
                keys.dedup();
                keys.truncate(request.limit);
                merged.extend(keys);
            }
            merged.sort();
            merged.truncate(request.limit);
            merged
        }))
    }
    async fn read_range(
        &mut self,
        table: &Table,
        first: Key,
        last: Key,
        _time_zone: &str,
    ) -> Result<Vec<RawRow>> {
        self.database.fault()?;
        Ok(self.database.with(|state| {
            state.range_queries += 1;
            let mut rows: Vec<_> = state
                .tables
                .iter()
                .find(|t| t.capture.capture == table.capture)
                .map(|memory| {
                    memory
                        .rows
                        .iter()
                        .filter(|(key, ..)| {
                            (key.lsn, key.sequence) >= (first.lsn, first.sequence)
                                && (key.lsn, key.sequence) <= (last.lsn, last.sequence)
                        })
                        .map(|(key, operation, commit, properties)| RawRow {
                            lsn: key.lsn,
                            sequence: key.sequence,
                            operation: *operation,
                            commit_time: *commit,
                            properties: properties.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            rows.sort_by_key(|row| (row.lsn, row.sequence, row.operation));
            rows
        }))
    }
    async fn erase_markers(&mut self, _barrier: &super::TableName, prefix: &str) -> Result<u64> {
        self.database.fault()?;
        Ok(self.database.with(|state| match &mut state.markers {
            Some(markers) => {
                let before = markers.len();
                markers.retain(|token| !super::markers::owns(prefix, token));
                (before - markers.len()) as u64
            }
            None => 0,
        }))
    }
    async fn close(self: Box<Self>) -> Result<()> {
        self.database.with(|state| state.closed_sessions += 1);
        Ok(())
    }
}
