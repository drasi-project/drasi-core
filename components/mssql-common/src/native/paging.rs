// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bounded CDC pages over a [`CdcSession`].
//!
//! A poll reads one page:
//!
//! 1. If the catalog fingerprint changed, reread the catalog; a binding
//!    change (capture instance, keys, table set) is `SchemaChanged`.
//! 2. Read the maximum LSN and each capture instance's retained minimum, and
//!    check the cursor: same binding, not ahead of CDC, still retained.
//! 3. Read at most `maxRows + 1` change group keys after the cursor, merged
//!    across tables in `(lsn, seqval, table)` order by the server.
//! 4. Fetch each table's rows for its keys with one range query per table
//!    (not one query per group), decode the groups and fill the page until
//!    `maxRows` changes or `maxBytes` of record payload.
//! 5. Recheck retention; only then is the page's boundary returned.
//!
//! The boundary is the maximum LSN only when the key read proved the
//! interval exhausted and every change fit; otherwise it is the last emitted
//! change's row position, so a page never skips an unread change.

use super::{
    catalog::{read_catalog, Catalog},
    engine::{check_group, decode_rows, element, CdcRow, Change, Poll},
    error::{position_unavailable, schema_changed},
    session::{CdcSession, Key, KeyRequest},
    Cursor, NativeSettings,
};
use anyhow::{ensure, Context, Result};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

/// Page bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageLimits {
    /// Most change groups read and changes emitted per page.
    pub max_rows: usize,
    /// Most record payload bytes per page; one change must fit.
    pub max_bytes: usize,
}

impl PageLimits {
    pub fn of(settings: &NativeSettings) -> Self {
        Self {
            max_rows: settings.max_rows,
            max_bytes: settings.max_bytes,
        }
    }
}

struct Page {
    rows: Vec<Change>,
    bytes: usize,
    limits: PageLimits,
}

impl Page {
    fn new(limits: PageLimits) -> Self {
        Self {
            rows: Vec::new(),
            bytes: 0,
            limits,
        }
    }
    fn full(&self) -> bool {
        self.rows.len() >= self.limits.max_rows || self.bytes >= self.limits.max_bytes
    }
    /// Adds `change`, or returns `false` when the page is full.
    fn push(&mut self, change: Change) -> Result<bool> {
        if self.full() {
            return Ok(false);
        }
        let size = change.size()?;
        if size > self.limits.max_bytes {
            ensure!(
                !self.rows.is_empty(),
                "one indivisible SQL Server change ({size} bytes) exceeds maxBytes; raise maxBytes"
            );
            return Ok(false);
        }
        if size > self.limits.max_bytes - self.bytes {
            return Ok(false);
        }
        if let Some(previous) = self.rows.last() {
            ensure!(
                change.position.compare(&previous.position)? == Ordering::Greater,
                "SQL Server CDC page contains duplicate or regressing positions"
            );
        }
        self.bytes += size;
        self.rows.push(change);
        Ok(true)
    }
    fn finish(self, binding: [u8; 32], max: [u8; 10], exhausted: bool) -> Result<Poll> {
        let boundary = if exhausted {
            Cursor::boundary(binding, max)
        } else {
            self.rows
                .last()
                .context("bounded SQL Server page made no progress")?
                .position
                .clone()
        };
        Ok(Poll {
            changes: self.rows,
            boundary,
        })
    }
}

/// The catalog, its fingerprint and the commit-time zone of one activation.
#[derive(Debug, Clone)]
pub struct Poller {
    pub source: String,
    pub catalog: Catalog,
    pub fingerprint: String,
    pub time_zone: String,
}

impl Poller {
    /// Reads the catalog (checking change-table permissions), its
    /// fingerprint and the commit-time zone.
    pub async fn open(
        session: &mut dyn CdcSession,
        settings: &NativeSettings,
        source: &str,
    ) -> Result<Self> {
        let fingerprint = session.catalog_fingerprint().await?;
        let catalog = read_catalog(session, settings, true).await?;
        let time_zone = session
            .time_zone(settings.server_time_zone.as_deref())
            .await?;
        for table in &catalog.tables {
            for column in table.unmapped() {
                log::info!(
                    "SQL Server column {}.{} ({}) is streamed {} without a schema type",
                    table.canonical(),
                    column.name,
                    column.type_name,
                    if column.kind == super::values::ColumnKind::Unmapped {
                        "as NULL"
                    } else {
                        "as text"
                    }
                );
            }
        }
        Ok(Self {
            source: source.to_owned(),
            catalog,
            fingerprint,
            time_zone,
        })
    }

    /// Rereads the catalog when `force` or its fingerprint changed. A new
    /// binding is `SchemaChanged`; other differences (non-key columns) are
    /// adopted.
    pub async fn refresh(
        &mut self,
        session: &mut dyn CdcSession,
        settings: &NativeSettings,
        force: bool,
    ) -> Result<()> {
        let fingerprint = session.catalog_fingerprint().await?;
        if !force && fingerprint == self.fingerprint {
            return Ok(());
        }
        let catalog = read_catalog(session, settings, true).await?;
        if catalog.binding != self.catalog.binding {
            return Err(schema_changed(
                "a selected table's capture instance, key columns or key types, or the set of \
                 selected CDC tables, changed",
            ));
        }
        self.catalog = catalog;
        self.fingerprint = fingerprint;
        Ok(())
    }

    /// The cursor a group without any committed cursor starts from:
    /// `Some(maximum LSN)` for `current`, `None` (each capture instance's
    /// retained minimum) for `beginning`.
    pub async fn origin(
        &self,
        session: &mut dyn CdcSession,
        settings: &NativeSettings,
    ) -> Result<Option<Cursor>> {
        Ok(match settings.start_position {
            super::StartPosition::Current => Some(Cursor::boundary(
                self.catalog.binding,
                max_lsn(session).await?,
            )),
            super::StartPosition::Beginning => None,
        })
    }

    /// The maximum LSN and each capture instance's retained minimum, checking
    /// that every instance is available and that `cursor` is neither ahead of
    /// CDC nor behind its retention.
    async fn retained(
        &self,
        session: &mut dyn CdcSession,
        cursor: Option<&Cursor>,
    ) -> Result<([u8; 10], Vec<[u8; 10]>)> {
        let catalog = &self.catalog;
        let captures: Vec<&str> = catalog.tables.iter().map(|t| t.capture.as_str()).collect();
        let max = max_lsn(session).await?;
        let mins = session.min_lsns(&captures).await?;
        ensure!(
            mins.len() == catalog.tables.len(),
            "SQL Server returned {} minimum LSNs for {} capture instances",
            mins.len(),
            catalog.tables.len()
        );
        for (table, min) in catalog.tables.iter().zip(&mins) {
            if *min == [0; 10] {
                return Err(position_unavailable(format!(
                    "capture instance {} of {} is unavailable",
                    table.capture,
                    table.canonical()
                )));
            }
        }
        if let Some(cursor) = cursor {
            if cursor.lsn > max {
                return Err(position_unavailable(
                    "the committed cursor is ahead of the CDC maximum LSN (the database was \
                     restored or CDC was re-enabled)",
                ));
            }
            for (table, min) in catalog.tables.iter().zip(&mins) {
                if cursor.lsn < *min {
                    return Err(position_unavailable(format!(
                        "CDC cleanup removed changes of {} after the committed cursor",
                        table.canonical()
                    )));
                }
            }
        }
        Ok((max, mins))
    }

    /// Checks that `cursor` belongs to this catalog's binding.
    pub fn check_binding(&self, cursor: &Cursor) -> Result<()> {
        cursor.validate()?;
        if cursor.binding != self.catalog.binding {
            return Err(schema_changed(
                "the committed cursor was taken under a different database, capture instance or \
                 key definition",
            ));
        }
        Ok(())
    }

    /// Reads one page after `cursor` (`None`: from each capture instance's
    /// retained minimum).
    pub async fn poll(
        &mut self,
        session: &mut dyn CdcSession,
        settings: &NativeSettings,
        cursor: Option<&Cursor>,
    ) -> Result<Poll> {
        self.refresh(session, settings, false).await?;
        if let Some(cursor) = cursor {
            self.check_binding(cursor)?;
        }
        let limits = PageLimits::of(settings);
        // Without a cursor the starts are the retained minimums, which the
        // cleanup job may raise while the page is read: retry instead of
        // reporting a gap nobody has committed past.
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.read_page(session, limits, cursor).await {
                Err(error) if cursor.is_none() && attempt < 3 && is_retention_move(&error) => {
                    log::debug!("SQL Server CDC retention moved during an initial page; rereading");
                }
                result => return result,
            }
        }
    }

    async fn read_page(
        &self,
        session: &mut dyn CdcSession,
        limits: PageLimits,
        cursor: Option<&Cursor>,
    ) -> Result<Poll> {
        let catalog = &self.catalog;
        let (max, mins) = self.retained(session, cursor).await?;
        let starts: Vec<[u8; 10]> = match cursor {
            Some(cursor) => vec![cursor.lsn; mins.len()],
            None => mins.clone(),
        };
        // A row cursor's own group is read again for its remaining images;
        // it does not count toward the page's group window.
        let extra = usize::from(cursor.is_some_and(|cursor| cursor.row.is_some()));
        let window = limits.max_rows.saturating_add(extra);
        let mut keys = session
            .next_keys(KeyRequest {
                tables: &catalog.tables,
                starts: &starts,
                max,
                after: cursor,
                limit: window.saturating_add(1),
            })
            .await?;
        ensure!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "SQL Server CDC key merge is not strictly ordered"
        );
        let own_group = cursor
            .and_then(|cursor| {
                cursor
                    .row
                    .map(|(sequence, table, _)| (cursor.lsn, sequence, table))
            })
            .is_some_and(|(lsn, sequence, table)| {
                keys.first()
                    .is_some_and(|key| (key.lsn, key.sequence, key.table) == (lsn, sequence, table))
            });
        let window = if own_group { window } else { limits.max_rows };
        let exhausted = keys.len() <= window;
        keys.truncate(window);
        let changes = self.read_groups(session, &keys).await;
        let changes = match changes {
            Ok(changes) => changes,
            Err(error) => {
                // A vanished group usually means cleanup ran; report that.
                self.validate_retention(session, &starts).await?;
                return Err(error);
            }
        };
        let mut page = Page::new(limits);
        let mut complete = true;
        for change in changes {
            if cursor.is_some_and(|cursor| {
                change
                    .position
                    .compare(cursor)
                    .is_ok_and(|order| order != Ordering::Greater)
            }) {
                continue;
            }
            if !page.push(change)? {
                complete = false;
                break;
            }
        }
        self.validate_retention(session, &starts).await?;
        if complete && exhausted {
            return page.finish(catalog.binding, max, true);
        }
        page.finish(catalog.binding, max, false)
    }

    async fn read_groups(&self, session: &mut dyn CdcSession, keys: &[Key]) -> Result<Vec<Change>> {
        let mut by_table: BTreeMap<i32, (Key, Key)> = BTreeMap::new();
        for key in keys {
            by_table
                .entry(key.table)
                .and_modify(|range| range.1 = *key)
                .or_insert((*key, *key));
        }
        let wanted: BTreeSet<Key> = keys.iter().copied().collect();
        let mut groups: BTreeMap<Key, Vec<(i32, drasi_core_types::models::Element)>> =
            BTreeMap::new();
        for (object, (first, last)) in by_table {
            let table = self
                .catalog
                .table(object)
                .with_context(|| format!("CDC key names unselected table object {object}"))?;
            for row in session
                .read_range(table, first, last, &self.time_zone)
                .await?
            {
                let key = Key {
                    lsn: row.lsn,
                    sequence: row.sequence,
                    table: object,
                };
                if !wanted.contains(&key) {
                    // A group beyond the key window (another table's keys
                    // interleave); it is read on a later page.
                    continue;
                }
                let timestamp = row.commit_time.timestamp_millis();
                ensure!(timestamp >= 0, "SQL Server commit time predates the epoch");
                let node = element(&self.source, table, row.properties, timestamp as u64)?;
                groups.entry(key).or_default().push((row.operation, node));
            }
        }
        let mut changes = Vec::new();
        for key in keys {
            let rows = groups
                .remove(key)
                .context("a SQL Server CDC change group disappeared while its page was read")?;
            let table = self.catalog.table(key.table).context("CDC group table")?;
            let operations: Vec<i32> = rows.iter().map(|(operation, _)| *operation).collect();
            check_group(&operations, table)?;
            let raw: Vec<CdcRow> = rows
                .into_iter()
                .map(|(operation, node)| (key.lsn, key.sequence, key.table, operation, node))
                .collect();
            changes.extend(decode_rows(raw, self.catalog.binding)?);
        }
        Ok(changes)
    }

    async fn validate_retention(
        &self,
        session: &mut dyn CdcSession,
        starts: &[[u8; 10]],
    ) -> Result<()> {
        let captures: Vec<&str> = self
            .catalog
            .tables
            .iter()
            .map(|t| t.capture.as_str())
            .collect();
        let mins = session.min_lsns(&captures).await?;
        for ((table, start), min) in self.catalog.tables.iter().zip(starts).zip(mins) {
            if min == [0; 10] || min > *start {
                return Err(position_unavailable(format!(
                    "CDC cleanup moved past the page being read from {}; nothing was emitted",
                    table.canonical()
                )));
            }
        }
        Ok(())
    }
}

fn is_retention_move(error: &anyhow::Error) -> bool {
    matches!(
        drasi_computation_contracts::source_failure(error),
        Some(drasi_computation_contracts::SourceFailure::PositionUnavailable(_))
    )
}

async fn max_lsn(session: &mut dyn CdcSession) -> Result<[u8; 10]> {
    let max = session.max_lsn().await?;
    ensure!(
        max != [0; 10],
        "SQL Server CDC has no maximum LSN; check that CDC is enabled and the capture job runs"
    );
    Ok(max)
}

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;
