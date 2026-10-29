// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    catalog::SqlClient,
    engine::{decode_rows, element, element_size, lsn, CdcRow, Change, Poll},
    quoted, Catalog, Cursor, NativeConfig, Table,
};
use crate::{Lsn, MsSqlConnection, StartPosition};
use anyhow::{ensure, Context, Result};
use futures_util::TryStreamExt;
use std::cmp::Ordering;

// Bound one worker's round trips independently of the caller's larger queue limit.
const KEY_WINDOW: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    lsn: [u8; 10],
    sequence: [u8; 10],
    table: i32,
}

struct Page {
    rows: Vec<Change>,
    bytes: usize,
    max_rows: usize,
    max_bytes: usize,
}
impl Page {
    fn new(config: &NativeConfig) -> Self {
        Self {
            rows: Vec::new(),
            bytes: 0,
            max_rows: config.max_rows,
            max_bytes: config.max_bytes,
        }
    }
    fn full(&self) -> bool {
        self.rows.len() >= self.max_rows || self.bytes >= self.max_bytes
    }
    fn push(&mut self, change: Change) -> Result<bool> {
        if self.full() {
            return Ok(false);
        }
        let size = change.size()?;
        if size > self.max_bytes {
            ensure!(!self.rows.is_empty(),
                "one indivisible SQL Server change exceeds max_bytes; increase the per-change/page byte budget");
            return Ok(false);
        }
        if self.rows.len() >= self.max_rows || size > self.max_bytes - self.bytes {
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
    fn finish(self, catalog: Catalog, max: [u8; 10], exhausted: bool) -> Result<Poll> {
        let boundary = if exhausted {
            Cursor::boundary(catalog.binding, max)
        } else {
            self.rows
                .last()
                .context("bounded SQL Server page made no progress")?
                .position
                .clone()
        };
        Ok(Poll {
            catalog,
            changes: self.rows,
            boundary,
        })
    }
}

/// Merge bounded per-table key windows in SQL, then fetch one complete CDC image
/// group at a time. Only a proven exhausted interval may advance to its max LSN.
pub(super) async fn poll(
    config: &NativeConfig,
    source: &str,
    resume: Option<&Cursor>,
) -> Result<Poll> {
    let work_deadline = tokio::time::Instant::now() + config.timeout() / 2;
    let mut connection = MsSqlConnection::connect(&config.connection).await?;
    let client = connection.client_mut();
    let catalog = Catalog::read(client, config).await?;
    let max = lsn(client, "SELECT sys.fn_cdc_get_max_lsn()", &[]).await?;
    if let Some(resume) = resume {
        resume.compare(&Cursor::boundary(catalog.binding, max))?;
        ensure!(
            resume.lsn <= max,
            "SQL Server checkpoint is ahead of retained CDC; explicit reset required"
        );
    }
    let mut starts = Vec::new();
    for table in &catalog.tables {
        let min = lsn(
            client,
            "SELECT sys.fn_cdc_get_min_lsn(@P1)",
            &[&table.capture],
        )
        .await?;
        ensure!(
            min != [0; 10],
            "SQL Server CDC capture instance is unavailable"
        );
        if let Some(resume) = resume {
            ensure!(
                resume.lsn >= min,
                "SQL Server CDC retention gap for {}; explicit query reset required",
                table.canonical()
            );
        }
        starts.push(resume.map_or(min, |position| position.lsn));
    }
    if resume.is_none() && config.connection.start_position == StartPosition::Current {
        return Ok(Poll {
            boundary: Cursor::boundary(catalog.binding, max),
            catalog,
            changes: Vec::new(),
        });
    }
    let mut page = Page::new(config);
    if let Some(position) = resume.filter(|position| position.row.is_some()) {
        let (sequence, object, _) = position.row.context("partial CDC position")?;
        let table = catalog
            .tables
            .iter()
            .find(|table| table.object == object)
            .context("checkpoint table is no longer selected")?;
        let group = read_group(
            client,
            config,
            source,
            table,
            Key {
                lsn: position.lsn,
                sequence,
                table: object,
            },
            catalog.binding,
        )
        .await?;
        ensure!(
            group.iter().any(|change| change.position == *position),
            "SQL Server partial CDC checkpoint row is no longer retained; explicit reset required"
        );
        for change in group {
            if change.position.compare(position)? == Ordering::Greater && !page.push(change)? {
                validate_retention(client, &catalog, &starts).await?;
                return page.finish(catalog, max, false);
            }
        }
        if page.full() || (!page.rows.is_empty() && tokio::time::Instant::now() >= work_deadline) {
            validate_retention(client, &catalog, &starts).await?;
            return page.finish(catalog, max, false);
        }
    }
    let limit = config
        .max_rows
        .min(KEY_WINDOW)
        .min((config.max_bytes / 128).max(1));
    let keys = next_keys(client, &catalog, &starts, max, resume, limit + 1).await?;
    let exhausted = keys.len() <= limit;
    for key in keys.into_iter().take(limit) {
        let table = catalog
            .tables
            .iter()
            .find(|table| table.object == key.table)
            .context("unknown CDC key table")?;
        for change in read_group(client, config, source, table, key, catalog.binding).await? {
            if !page.push(change)? {
                validate_retention(client, &catalog, &starts).await?;
                return page.finish(catalog, max, false);
            }
        }
        if page.full() || tokio::time::Instant::now() >= work_deadline {
            // Even the final group may have a later image or unseen key. The
            // next bounded read proves exhaustion before publishing a max cut.
            validate_retention(client, &catalog, &starts).await?;
            return page.finish(catalog, max, false);
        }
    }
    validate_retention(client, &catalog, &starts).await?;
    page.finish(catalog, max, exhausted)
}

async fn validate_retention(
    client: &mut SqlClient,
    catalog: &Catalog,
    starts: &[[u8; 10]],
) -> Result<()> {
    for (table, start) in catalog.tables.iter().zip(starts) {
        let min = lsn(
            client,
            "SELECT sys.fn_cdc_get_min_lsn(@P1)",
            &[&table.capture],
        )
        .await?;
        ensure!(min != [0;10] && min <= *start,
            "SQL Server CDC retention moved past a page while reading {}; no page checkpoint advanced",table.canonical());
    }
    Ok(())
}

async fn next_keys(
    client: &mut SqlClient,
    catalog: &Catalog,
    starts: &[[u8; 10]],
    max: [u8; 10],
    resume: Option<&Cursor>,
    limit: usize,
) -> Result<Vec<Key>> {
    // Each branch has its own TOP. A table cannot hide an earlier key in the
    // global merge, and the server merge has at most table_count * limit keys.
    let mut branches = Vec::new();
    for (index, table) in catalog.tables.iter().enumerate() {
        let predicate = match resume.and_then(|position|position.row) {
            Some(_) => format!("AND (__$start_lsn>@P3 OR (__$start_lsn=@P3 AND (__$seqval>@P4 OR (__$seqval=@P4 AND {}>@P5))))",table.object),
            None if resume.is_some() => "AND __$start_lsn>@P3".into(),
            None => String::new(),
        };
        branches.push(format!(
            "SELECT * FROM (SELECT DISTINCT TOP (@P1) __$start_lsn,__$seqval,CAST({} AS int) AS object_id \
             FROM cdc.{} WHERE __$start_lsn>=@P{} AND __$start_lsn<=@P2 {} \
             ORDER BY __$start_lsn,__$seqval) AS table_{}",
            table.object,quoted(&format!("{}_CT",table.capture)),6+index,predicate,index));
    }
    let sql = format!(
        "SELECT TOP (@P1) __$start_lsn,__$seqval,object_id FROM ({}) AS merged \
        ORDER BY __$start_lsn,__$seqval,object_id",
        branches.join(" UNION ALL ")
    );
    let mut query = tiberius::Query::new(sql);
    query.bind(i32::try_from(limit)?);
    query.bind(max.to_vec());
    query.bind(resume.map_or([0; 10], |p| p.lsn).to_vec());
    query.bind(
        resume
            .and_then(|p| p.row)
            .map_or([0; 10], |row| row.0)
            .to_vec(),
    );
    query.bind(resume.and_then(|p| p.row).map_or(0, |row| row.1));
    for start in starts {
        query.bind(start.to_vec());
    }
    let mut rows = query.query(client).await?.into_row_stream();
    let mut keys = Vec::new();
    while let Some(row) = rows.try_next().await? {
        ensure!(
            keys.len() < limit,
            "SQL Server key page exceeded its requested bound"
        );
        let key = Key {
            lsn: *Lsn::from_bytes(row.try_get::<&[u8], _>(0)?.context("CDC key LSN")?)?.as_bytes(),
            sequence: *Lsn::from_bytes(row.try_get::<&[u8], _>(1)?.context("CDC key sequence")?)?
                .as_bytes(),
            table: row.try_get(2)?.context("CDC key table")?,
        };
        ensure!(
            keys.last().is_none_or(|previous| key > *previous),
            "CDC key merge is not strictly ordered"
        );
        keys.push(key);
    }
    Ok(keys)
}

async fn read_group(
    client: &mut SqlClient,
    config: &NativeConfig,
    source: &str,
    table: &Table,
    key: Key,
    binding: [u8; 32],
) -> Result<Vec<Change>> {
    let sql = format!(
        "SELECT TOP (3) *, DATEADD(MINUTE,DATEDIFF(MINUTE,GETDATE(),GETUTCDATE()),sys.fn_cdc_map_lsn_to_time(__$start_lsn)) AS __$commit_time \
         FROM cdc.{} WHERE __$start_lsn=@P1 AND __$seqval=@P2 ORDER BY __$operation",
        quoted(&format!("{}_CT",table.capture)));
    let mut rows = client
        .query(sql, &[&&key.lsn[..], &&key.sequence[..]])
        .await?
        .into_row_stream();
    let mut raw: Vec<CdcRow> = Vec::new();
    while let Some(row) = rows.try_next().await? {
        ensure!(
            raw.len() < 2,
            "ambiguous SQL Server CDC group; duplicate image positions"
        );
        ensure!(
            Lsn::from_bytes(
                row.try_get::<&[u8], _>("__$start_lsn")?
                    .context("CDC group LSN")?
            )?
            .as_bytes()
                == &key.lsn
                && Lsn::from_bytes(
                    row.try_get::<&[u8], _>("__$seqval")?
                        .context("CDC group sequence")?
                )?
                .as_bytes()
                    == &key.sequence,
            "SQL Server CDC group returned a different source position"
        );
        let operation: i32 = row.try_get("__$operation")?.context("CDC operation")?;
        let timestamp = row
            .try_get::<chrono::NaiveDateTime, _>("__$commit_time")?
            .context("CDC commit timestamp unavailable")?
            .and_utc()
            .timestamp_millis();
        ensure!(timestamp >= 0, "SQL Server commit time predates epoch");
        let element = element(source, table, &row, timestamp as u64)?;
        ensure!(
            element_size(&element)? <= config.max_bytes,
            "one SQL Server CDC image exceeds max_bytes"
        );
        raw.push((key.lsn, key.sequence, key.table, operation, element));
    }
    ensure!(
        !raw.is_empty(),
        "SQL Server CDC group disappeared during paging; no checkpoint advanced"
    );
    ensure!(
        matches!(
            raw.as_slice(),
            [(_, _, _, 1 | 2, _)] | [(_, _, _, 3, _), (_, _, _, 4, _)]
        ),
        "SQL Server CDC before/after pair is incomplete or ambiguous"
    );
    decode_rows(raw, binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use std::sync::Arc;
    fn config(rows: usize, bytes: usize) -> NativeConfig {
        serde_json::from_value(serde_json::json!({"connection":{"database":"d","user":"u"},"max_rows":rows,"max_bytes":bytes})).expect("fixture")
    }
    fn change(lsn: u8, seq: u8, table: i32, ordinal: u8) -> Change {
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", ElementValue::Integer(i64::from(seq)));
        Change {
            change: SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("s", &format!("{table}:{seq}")),
                        labels: Arc::from([Arc::from("Items")]),
                        effective_from: 1,
                    },
                    properties,
                },
            },
            before: None,
            position: Cursor {
                version: 1,
                binding: [1; 32],
                lsn: [lsn; 10],
                row: Some(([seq; 10], table, ordinal)),
            },
        }
    }
    fn catalog() -> Catalog {
        Catalog {
            tables: Vec::new(),
            binding: [1; 32],
        }
    }
    #[test]
    fn one_row_pages_preserve_large_backlog_and_never_jump_to_max() -> Result<()> {
        let mut handled = None;
        for sequence in 1..=200 {
            let mut page = Page::new(&config(1, 4096));
            assert!(page.push(change(1, sequence, 7, 0))?);
            assert!(!page.push(change(2, 0, 7, 0))?);
            let poll = page.finish(catalog(), [9; 10], false)?;
            assert_eq!(poll.changes.len(), 1);
            assert_eq!(poll.boundary.lsn, [1; 10]);
            if let Some(previous) = handled {
                assert_eq!(poll.boundary.compare(&previous)?, Ordering::Greater);
            }
            handled = Some(poll.boundary);
        }
        let end = Page::new(&config(1, 4096)).finish(catalog(), [9; 10], true)?;
        assert!(end.changes.is_empty());
        assert!(end.boundary.row.is_none());
        Ok(())
    }
    #[test]
    fn bytes_are_a_page_boundary_not_a_total_history_limit() -> Result<()> {
        let size = change(1, 1, 7, 0).size()?;
        let mut page = Page::new(&config(100, size));
        assert!(page.push(change(1, 1, 7, 0))?);
        assert!(!page.push(change(1, 2, 7, 0))?);
        let mut next = Page::new(&config(100, size));
        assert!(next.push(change(1, 2, 7, 0))?);
        assert!(Page::new(&config(1, size - 1))
            .push(change(1, 1, 7, 0))
            .is_err());
        Ok(())
    }
    #[test]
    fn a_rekey_pair_can_resume_between_images_but_an_update_stays_one_change() -> Result<()> {
        let SourceChange::Insert { element: before } = change(1, 1, 7, 0).change else {
            panic!("before")
        };
        let SourceChange::Insert { element: after } = change(1, 2, 7, 0).change else {
            panic!("after")
        };
        let pair = vec![
            ([1; 10], [1; 10], 7, 3, before.clone()),
            ([1; 10], [1; 10], 7, 4, after),
        ];
        let mut decoded = decode_rows(pair.clone(), [1; 32])?.into_iter();
        let mut page = Page::new(&config(1, 4096));
        assert!(page.push(decoded.next().context("delete")?)?);
        assert!(!page.push(decoded.next().context("insert")?)?);
        let resume = page.finish(catalog(), [9; 10], false)?.boundary;
        let recovered = decode_rows(pair, [1; 32])?
            .into_iter()
            .filter(|row| row.position.compare(&resume).expect("same binding") == Ordering::Greater)
            .collect::<Vec<_>>();
        assert_eq!(recovered.len(), 1);
        assert!(matches!(recovered[0].change, SourceChange::Insert { .. }));
        assert_eq!(recovered[0].position.row, Some(([1; 10], 7, 1)));
        let update = decode_rows(
            vec![
                ([1; 10], [1; 10], 7, 3, before.clone()),
                ([1; 10], [1; 10], 7, 4, before),
            ],
            [1; 32],
        )?;
        assert_eq!(update.len(), 1);
        let size = update[0].size()?;
        assert!(Page::new(&config(1, size)).push(update.into_iter().next().context("update")?)?);
        Ok(())
    }
    #[test]
    fn global_group_order_and_rekey_image_order_are_independent_of_table_iteration() -> Result<()> {
        let mut keys = [
            Key {
                lsn: [2; 10],
                sequence: [1; 10],
                table: 2,
            },
            Key {
                lsn: [1; 10],
                sequence: [2; 10],
                table: 8,
            },
            Key {
                lsn: [1; 10],
                sequence: [1; 10],
                table: 9,
            },
            Key {
                lsn: [1; 10],
                sequence: [1; 10],
                table: 3,
            },
        ];
        keys.sort();
        assert_eq!(
            keys.iter().map(|key| key.table).collect::<Vec<_>>(),
            [3, 9, 8, 2]
        );
        let mut first = Page::new(&config(1, 4096));
        first.push(change(1, 1, 3, 0))?;
        assert!(!first.push(change(1, 1, 3, 1))?);
        let old = first.finish(catalog(), [9; 10], false)?.boundary;
        let mut second = Page::new(&config(1, 4096));
        second.push(change(1, 1, 3, 1))?;
        assert_eq!(
            second
                .finish(catalog(), [9; 10], false)?
                .boundary
                .compare(&old)?,
            Ordering::Greater
        );
        Ok(())
    }
}
