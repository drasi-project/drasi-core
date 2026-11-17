// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The Tiberius implementation of [`CdcSession`].
//!
//! Every value is a bound parameter; object names come from the catalog and
//! are bracket-quoted. Tiberius cannot send a TDS ATTENTION, so a statement
//! cannot be cancelled in place: the per-round-trip command timeout and
//! dropping the session both close the connection.
//!
//! Live verification of this SQL against SQL Server is deferred (CW-78).

use super::{
    catalog::Table,
    error::unsupported,
    identifiers::quoted,
    session::{
        CaptureInstance, CdcConnector, CdcSession, DatabaseIdentity, Key, KeyRequest, RawColumn,
        RawRow, TableFacts,
    },
    values::{binary_text, json_value, ColumnKind},
    NativeSettings,
};
use crate::client::{connect_within, Authentication, SqlClient};
use crate::Lsn;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use drasi_core_types::models::{ElementPropertyMap, ElementValue};
use futures_util::TryStreamExt;
use std::{sync::Arc, time::Duration};
use tiberius::{Query, Row};

/// The column type expression: alias types resolve to their base type, CLR
/// types (geography, geometry, hierarchyid) keep their own name.
const TYPE_NAME: &str = "CASE WHEN ty.is_user_defined = 1 AND ty.is_assembly_type = 0 \
     THEN TYPE_NAME(c.system_type_id) ELSE ty.name END";

/// Opens Tiberius sessions for one configuration and authentication.
pub struct TiberiusConnector {
    config: tiberius::Config,
    connect_timeout: Duration,
}

impl TiberiusConnector {
    pub fn new(settings: &NativeSettings, authentication: &Authentication) -> Self {
        Self {
            config: settings.connection().tiberius_config(authentication),
            connect_timeout: settings.connect_timeout(),
        }
    }

    /// Opens one raw client.
    pub async fn client(&self) -> Result<SqlClient> {
        connect_within(self.config.clone(), self.connect_timeout).await
    }
}

#[async_trait]
impl CdcConnector for TiberiusConnector {
    async fn connect(&self) -> Result<Box<dyn CdcSession>> {
        Ok(Box::new(TiberiusSession::new(self.client().await?)))
    }
}

/// One Tiberius connection.
pub struct TiberiusSession {
    client: SqlClient,
}

impl TiberiusSession {
    pub fn new(client: SqlClient) -> Self {
        Self { client }
    }

    /// The raw client, for snapshot transactions.
    pub fn client_mut(&mut self) -> &mut SqlClient {
        &mut self.client
    }

    async fn rows(&mut self, query: Query<'_>) -> Result<Vec<Row>> {
        Ok(query
            .query(&mut self.client)
            .await?
            .into_first_result()
            .await?)
    }

    async fn columns(&mut self, object: i32) -> Result<Vec<RawColumn>> {
        let mut query = Query::new(format!(
            "SELECT c.name, {TYPE_NAME}, c.precision, c.scale FROM sys.columns c \
             JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
             WHERE c.object_id = @P1 AND LEFT(c.name, 3) <> '__$' ORDER BY c.column_id"
        ));
        query.bind(object);
        self.rows(query)
            .await?
            .iter()
            .map(|row| {
                Ok(RawColumn {
                    name: text(row, 0)?,
                    type_name: text(row, 1)?,
                    precision: row.try_get::<u8, _>(2)?.unwrap_or(0),
                    scale: row.try_get::<u8, _>(3)?.unwrap_or(0),
                })
            })
            .collect()
    }
}

fn text(row: &Row, index: usize) -> Result<String> {
    Ok(row
        .try_get::<&str, _>(index)?
        .with_context(|| format!("SQL Server returned NULL in column {index}"))?
        .to_owned())
}

fn lsn_at(row: &Row, index: usize) -> Result<[u8; 10]> {
    match row.try_get::<&[u8], _>(index)? {
        Some(bytes) => Ok(*Lsn::from_bytes(bytes)?.as_bytes()),
        None => Ok([0; 10]),
    }
}

#[async_trait]
impl CdcSession for TiberiusSession {
    async fn database(&mut self) -> Result<DatabaseIdentity> {
        let rows = self
            .rows(Query::new(
                "SELECT CONVERT(nvarchar(36), service_broker_guid), DB_NAME() \
                 FROM sys.databases WHERE database_id = DB_ID()",
            ))
            .await?;
        let row = rows
            .first()
            .context("SQL Server database identity missing")?;
        Ok(DatabaseIdentity {
            guid: text(row, 0)?,
            name: text(row, 1)?,
        })
    }

    async fn capture_instances(&mut self) -> Result<Vec<CaptureInstance>> {
        self.rows(Query::new(
            "SELECT s.name, t.name, t.object_id, ct.object_id, ct.capture_instance \
             FROM cdc.change_tables ct JOIN sys.tables t ON t.object_id = ct.source_object_id \
             JOIN sys.schemas s ON s.schema_id = t.schema_id \
             ORDER BY s.name, t.name, ct.capture_instance",
        ))
        .await
        .context("reading cdc.change_tables; the login needs SELECT on the CDC catalog")?
        .iter()
        .map(|row| {
            Ok(CaptureInstance {
                schema: text(row, 0)?,
                table: text(row, 1)?,
                object: row.try_get(2)?.context("missing table object")?,
                capture_object: row.try_get(3)?.context("missing capture object")?,
                capture: text(row, 4)?,
            })
        })
        .collect()
    }

    async fn table_facts(
        &mut self,
        object: i32,
        capture_object: Option<i32>,
    ) -> Result<TableFacts> {
        let source_columns = self.columns(object).await?;
        let columns = match capture_object {
            Some(capture) => self.columns(capture).await?,
            None => source_columns.clone(),
        };
        let mut keys = Query::new(
            "SELECT c.name FROM sys.indexes i \
             JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE i.object_id = @P1 AND i.is_primary_key = 1 ORDER BY ic.key_ordinal",
        );
        keys.bind(object);
        let primary_key = self
            .rows(keys)
            .await?
            .iter()
            .map(|row| text(row, 0))
            .collect::<Result<_>>()?;
        Ok(TableFacts {
            columns,
            source_columns,
            primary_key,
        })
    }

    async fn find_table(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<Option<(i32, String, String)>> {
        let mut query = Query::new(
            "SELECT t.object_id, s.name, t.name FROM sys.tables t \
             JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE s.name = @P1 AND t.name = @P2",
        );
        query.bind(schema.to_owned());
        query.bind(table.to_owned());
        let rows = self.rows(query).await?;
        rows.first()
            .map(|row| {
                Ok((
                    row.try_get(0)?.context("missing table object")?,
                    text(row, 1)?,
                    text(row, 2)?,
                ))
            })
            .transpose()
    }

    async fn can_select(&mut self, object: &str) -> Result<bool> {
        let mut query =
            Query::new("SELECT CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'SELECT') AS int)");
        query.bind(object.to_owned());
        let rows = self.rows(query).await?;
        Ok(rows
            .first()
            .and_then(|row| row.try_get::<i32, _>(0).ok().flatten())
            == Some(1))
    }

    async fn catalog_fingerprint(&mut self) -> Result<String> {
        let rows = self
            .rows(Query::new(
                "SELECT CONCAT((SELECT COUNT_BIG(*) FROM cdc.change_tables), ':', \
                 (SELECT CHECKSUM_AGG(CHECKSUM(object_id, source_object_id, capture_instance)) \
                  FROM cdc.change_tables), ':', \
                 (SELECT CONVERT(varchar(30), MAX(o.modify_date), 126) FROM sys.objects o \
                  JOIN cdc.change_tables ct ON ct.source_object_id = o.object_id))",
            ))
            .await?;
        text(rows.first().context("missing catalog fingerprint")?, 0)
    }

    async fn time_zone(&mut self, configured: Option<&str>) -> Result<String> {
        if let Some(zone) = configured {
            let mut query = Query::new("SELECT COUNT(*) FROM sys.time_zone_info WHERE name = @P1");
            query.bind(zone.to_owned());
            let rows = self.rows(query).await?;
            let found: i32 = rows
                .first()
                .and_then(|row| row.try_get(0).ok().flatten())
                .unwrap_or(0);
            if found != 1 {
                return Err(unsupported(format!(
                    "serverTimeZone {zone:?} is not a sys.time_zone_info name"
                )));
            }
            return Ok(zone.to_owned());
        }
        match self
            .rows(Query::new("SELECT CURRENT_TIMEZONE_ID()"))
            .await
        {
            Ok(rows) => text(rows.first().context("missing time zone")?, 0),
            Err(error)
                if error
                    .downcast_ref::<tiberius::error::Error>()
                    .is_some_and(|e| matches!(e, tiberius::error::Error::Server(t) if t.code() == 195)) =>
            {
                Err(unsupported(
                    "this server cannot report its time zone (CURRENT_TIMEZONE_ID needs SQL Server \
                     2022 or Azure SQL); set serverTimeZone to its sys.time_zone_info name, for \
                     example \"UTC\" or \"Pacific Standard Time\"",
                ))
            }
            Err(error) => Err(error),
        }
    }

    async fn max_lsn(&mut self) -> Result<[u8; 10]> {
        let rows = self
            .rows(Query::new("SELECT sys.fn_cdc_get_max_lsn()"))
            .await?;
        lsn_at(rows.first().context("missing CDC maximum LSN")?, 0)
    }

    async fn min_lsns(&mut self, captures: &[&str]) -> Result<Vec<[u8; 10]>> {
        if captures.is_empty() {
            return Ok(Vec::new());
        }
        let columns: Vec<String> = (1..=captures.len())
            .map(|index| format!("sys.fn_cdc_get_min_lsn(@P{index})"))
            .collect();
        let mut query = Query::new(format!("SELECT {}", columns.join(", ")));
        for capture in captures {
            query.bind(capture.to_string());
        }
        let rows = self.rows(query).await?;
        let row = rows.first().context("missing CDC minimum LSNs")?;
        (0..captures.len())
            .map(|index| lsn_at(row, index))
            .collect()
    }

    async fn next_keys(&mut self, request: KeyRequest<'_>) -> Result<Vec<Key>> {
        // Each branch has its own TOP, so a table cannot hide an earlier key
        // from the global merge.
        let row_cursor = request.after.and_then(|after| after.row);
        let mut branches = Vec::new();
        for (index, table) in request.tables.iter().enumerate() {
            let predicate = match (request.after, row_cursor) {
                (Some(_), Some(_)) => format!(
                    "AND (__$start_lsn > @P3 OR (__$start_lsn = @P3 AND (__$seqval > @P4 OR \
                     (__$seqval = @P4 AND {} >= @P5))))",
                    table.object
                ),
                (Some(_), None) => "AND __$start_lsn > @P3".to_owned(),
                (None, _) => String::new(),
            };
            branches.push(format!(
                "SELECT * FROM (SELECT DISTINCT TOP (@P1) __$start_lsn, __$seqval, \
                 CAST({} AS int) AS object_id FROM {} WHERE __$start_lsn >= @P{} AND \
                 __$start_lsn <= @P2 {predicate} ORDER BY __$start_lsn, __$seqval) AS table_{index}",
                table.object,
                table.change_table(),
                6 + index,
            ));
        }
        let mut query = Query::new(format!(
            "SELECT TOP (@P1) __$start_lsn, __$seqval, object_id FROM ({}) AS merged \
             ORDER BY __$start_lsn, __$seqval, object_id",
            branches.join(" UNION ALL ")
        ));
        query.bind(i64::try_from(request.limit)?);
        query.bind(request.max.to_vec());
        query.bind(request.after.map_or([0; 10], |after| after.lsn).to_vec());
        query.bind(row_cursor.map_or([0; 10], |row| row.0).to_vec());
        query.bind(row_cursor.map_or(0, |row| row.1));
        for start in request.starts {
            query.bind(start.to_vec());
        }
        let mut stream = query.query(&mut self.client).await?.into_row_stream();
        let mut keys = Vec::new();
        while let Some(row) = stream.try_next().await? {
            anyhow::ensure!(
                keys.len() < request.limit,
                "SQL Server key page exceeded its requested bound"
            );
            keys.push(Key {
                lsn: lsn_at(&row, 0)?,
                sequence: lsn_at(&row, 1)?,
                table: row.try_get(2)?.context("CDC key table")?,
            });
        }
        Ok(keys)
    }

    async fn read_range(
        &mut self,
        table: &Table,
        first: Key,
        last: Key,
        time_zone: &str,
    ) -> Result<Vec<RawRow>> {
        let mut query = Query::new(format!(
            "SELECT __$start_lsn, __$seqval, __$operation, \
             (sys.fn_cdc_map_lsn_to_time(__$start_lsn) AT TIME ZONE @P5) AS {}, {} FROM {} \
             WHERE (__$start_lsn > @P1 OR (__$start_lsn = @P1 AND __$seqval >= @P2)) \
             AND (__$start_lsn < @P3 OR (__$start_lsn = @P3 AND __$seqval <= @P4)) \
             ORDER BY __$start_lsn, __$seqval, __$operation",
            quoted("__$commit_time"),
            table.selection(),
            table.change_table()
        ));
        query.bind(first.lsn.to_vec());
        query.bind(first.sequence.to_vec());
        query.bind(last.lsn.to_vec());
        query.bind(last.sequence.to_vec());
        query.bind(time_zone.to_owned());
        let mut stream = query.query(&mut self.client).await?.into_row_stream();
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await? {
            rows.push(RawRow {
                lsn: lsn_at(&row, 0)?,
                sequence: lsn_at(&row, 1)?,
                operation: row.try_get::<i32, _>(2)?.context("CDC operation is NULL")?,
                commit_time: row
                    .try_get::<DateTime<FixedOffset>, _>(3)?
                    .context("CDC commit time unavailable")?,
                properties: properties(table, &row, 4)?,
            });
        }
        Ok(rows)
    }

    async fn erase_markers(&mut self, barrier: &super::TableName, prefix: &str) -> Result<u64> {
        super::markers::erase_markers(&mut self.client, barrier, prefix).await
    }

    async fn close(self: Box<Self>) -> Result<()> {
        self.client.close().await?;
        Ok(())
    }
}

/// Decodes `table`'s columns, which start at `offset` in `row`.
pub fn properties(table: &Table, row: &Row, offset: usize) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();
    for (index, column) in table.columns.iter().enumerate() {
        properties.insert(
            column.name.as_str(),
            decode(row, offset + index, column.kind)
                .with_context(|| format!("decoding {}.{}", table.canonical(), column.name))?,
        );
    }
    Ok(properties)
}

fn decode(row: &Row, index: usize, kind: ColumnKind) -> Result<ElementValue> {
    let string = |value: Option<&str>| {
        value.map_or(ElementValue::Null, |value| {
            ElementValue::String(Arc::from(value))
        })
    };
    Ok(match kind {
        ColumnKind::Integer => match row.try_get::<i64, _>(index) {
            Ok(value) => value.map_or(ElementValue::Null, ElementValue::Integer),
            Err(_) => match row.try_get::<i32, _>(index) {
                Ok(value) => value.map_or(ElementValue::Null, |v| ElementValue::Integer(v.into())),
                Err(_) => match row.try_get::<i16, _>(index) {
                    Ok(value) => {
                        value.map_or(ElementValue::Null, |v| ElementValue::Integer(v.into()))
                    }
                    Err(_) => row
                        .try_get::<u8, _>(index)?
                        .map_or(ElementValue::Null, |v| ElementValue::Integer(v.into())),
                },
            },
        },
        ColumnKind::Float => match row.try_get::<f64, _>(index) {
            Ok(value) => value.map_or(ElementValue::Null, |v| ElementValue::Float(v.into())),
            Err(_) => row
                .try_get::<f32, _>(index)?
                .map_or(ElementValue::Null, |v| {
                    ElementValue::Float(f64::from(v).into())
                }),
        },
        ColumnKind::Boolean => row
            .try_get::<bool, _>(index)?
            .map_or(ElementValue::Null, ElementValue::Bool),
        ColumnKind::LocalDateTime => row
            .try_get::<NaiveDateTime, _>(index)?
            .map_or(ElementValue::Null, ElementValue::LocalDateTime),
        ColumnKind::ZonedDateTime => row
            .try_get::<DateTime<FixedOffset>, _>(index)?
            .map_or(ElementValue::Null, ElementValue::ZonedDateTime),
        ColumnKind::Date => row
            .try_get::<NaiveDate, _>(index)?
            .map_or(ElementValue::Null, |v| {
                ElementValue::String(Arc::from(v.to_string()))
            }),
        ColumnKind::Time => row
            .try_get::<NaiveTime, _>(index)?
            .map_or(ElementValue::Null, |v| {
                ElementValue::String(Arc::from(v.to_string()))
            }),
        ColumnKind::Guid => row
            .try_get::<uuid::Uuid, _>(index)?
            .map_or(ElementValue::Null, |v| {
                ElementValue::String(Arc::from(v.to_string()))
            }),
        ColumnKind::Binary => row
            .try_get::<&[u8], _>(index)?
            .map_or(ElementValue::Null, binary_text),
        ColumnKind::Json => row
            .try_get::<&str, _>(index)?
            .map_or(ElementValue::Null, json_value),
        ColumnKind::Decimal | ColumnKind::Text | ColumnKind::Converted => {
            string(row.try_get::<&str, _>(index)?)
        }
        ColumnKind::Unmapped => ElementValue::Null,
    })
}
