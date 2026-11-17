// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The database seam of the native CDC engine.
//!
//! [`CdcSession`] is every question the poller asks SQL Server; the Tiberius
//! implementation lives in [`super::sql_session`]. Paging, cursor checks,
//! grouping, catalog binding and reconnect policy are written against the
//! trait, so they are unit tested with an in-memory session.

use super::{catalog::Table, Cursor};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};
use drasi_core_types::models::ElementPropertyMap;

/// One row of `cdc.change_tables` joined to its source table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureInstance {
    pub schema: String,
    pub table: String,
    pub object: i32,
    pub capture_object: i32,
    pub capture: String,
}

/// A column as the catalog reports it: name, base type, precision, scale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawColumn {
    pub name: String,
    pub type_name: String,
    pub precision: u8,
    pub scale: u8,
}

/// The catalog facts of one table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableFacts {
    /// Columns of the capture table (CDC) or the table itself (snapshot),
    /// in column order, without `__$` columns.
    pub columns: Vec<RawColumn>,
    /// Columns of the source table, for key column types.
    pub source_columns: Vec<RawColumn>,
    /// Primary key columns in key order (empty without a primary key).
    pub primary_key: Vec<String>,
}

/// The database a catalog belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseIdentity {
    /// `sys.databases.service_broker_guid`, stable across host renames.
    pub guid: String,
    /// `DB_NAME()`.
    pub name: String,
}

/// A CDC change group key: commit LSN, sequence value, table object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub lsn: [u8; 10],
    pub sequence: [u8; 10],
    pub table: i32,
}

/// One change-table row, decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct RawRow {
    pub lsn: [u8; 10],
    pub sequence: [u8; 10],
    pub operation: i32,
    /// Commit time with the server's offset at that instant.
    pub commit_time: DateTime<FixedOffset>,
    pub properties: ElementPropertyMap,
}

/// A request for the next change group keys.
#[derive(Clone, Copy, Debug)]
pub struct KeyRequest<'a> {
    pub tables: &'a [Table],
    /// Inclusive lower LSN bound per table (its retained minimum or the
    /// cursor's LSN).
    pub starts: &'a [[u8; 10]],
    /// Inclusive upper LSN bound.
    pub max: [u8; 10],
    /// Keys at or before this position are skipped; a row cursor's own group
    /// is included so its remaining images can be emitted.
    pub after: Option<&'a Cursor>,
    pub limit: usize,
}

/// One SQL Server connection, as the CDC engine uses it.
#[async_trait]
pub trait CdcSession: Send {
    /// The database identity.
    async fn database(&mut self) -> Result<DatabaseIdentity>;
    /// Every CDC capture instance with its source table.
    async fn capture_instances(&mut self) -> Result<Vec<CaptureInstance>>;
    /// Columns and keys of `object`; columns come from `capture_object`
    /// when given (the change table).
    async fn table_facts(&mut self, object: i32, capture_object: Option<i32>)
        -> Result<TableFacts>;
    /// Looks a table up by name: `(object_id, schema, table)` as cataloged.
    async fn find_table(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<Option<(i32, String, String)>>;
    /// Whether the login may SELECT from `object` (a quoted two-part name).
    async fn can_select(&mut self, object: &str) -> Result<bool>;
    /// A cheap fingerprint of the capture catalog and selected-table DDL;
    /// the catalog is reread only when it changes.
    async fn catalog_fingerprint(&mut self) -> Result<String>;
    /// The server time zone used for commit times: `configured` after
    /// checking it exists, otherwise the detected one.
    async fn time_zone(&mut self, configured: Option<&str>) -> Result<String>;
    /// `sys.fn_cdc_get_max_lsn()`.
    async fn max_lsn(&mut self) -> Result<[u8; 10]>;
    /// `sys.fn_cdc_get_min_lsn(capture)` for each capture, in order.
    async fn min_lsns(&mut self, captures: &[&str]) -> Result<Vec<[u8; 10]>>;
    /// At most `limit` keys after the request's position, globally ordered.
    async fn next_keys(&mut self, request: KeyRequest<'_>) -> Result<Vec<Key>>;
    /// Every row of `table` with `first <= (lsn, sequence) <= last`, ordered
    /// by LSN, sequence and operation.
    async fn read_range(
        &mut self,
        table: &Table,
        first: Key,
        last: Key,
        time_zone: &str,
    ) -> Result<Vec<RawRow>>;
    /// Deletes every barrier marker whose token starts with `prefix`
    /// (see [`super::markers`]); a missing barrier table deletes nothing.
    async fn erase_markers(&mut self, barrier: &super::TableName, prefix: &str) -> Result<u64>;
    /// Closes the connection gracefully.
    async fn close(self: Box<Self>) -> Result<()>;
}

/// Opens [`CdcSession`]s.
#[async_trait]
pub trait CdcConnector: Send + Sync {
    async fn connect(&self) -> Result<Box<dyn CdcSession>>;
}
