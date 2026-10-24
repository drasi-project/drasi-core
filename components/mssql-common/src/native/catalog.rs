// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{quoted, table_name, NativeConfig};
use crate::{MsSqlConnection, PrimaryKeyCache};
use anyhow::{ensure, Context, Result};
use drasi_core_types::schema::{NodeSchema, PropertySchema, PropertyType, SourceSchema};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use tiberius::Client;
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

pub type SqlClient = Client<Compat<TcpStream>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub object: i32,
    pub capture_object: i32,
    pub capture: String,
    // SQL Server advances this retention low watermark without changing capture
    // identity. Per-page min-LSN checks validate retention instead.
    #[serde(skip_serializing)]
    pub capture_start: Vec<u8>,
    pub columns: Vec<(String, String, i16, u8, u8, bool)>,
    pub keys: Vec<String>,
}
impl Table {
    pub fn canonical(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
    pub fn sql(&self) -> String {
        format!("{}.{}", quoted(&self.schema), quoted(&self.name))
    }
    pub fn selection(&self) -> String {
        self.columns
            .iter()
            .map(|c| quoted(&c.0))
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Clone)]
pub struct Catalog {
    pub tables: Vec<Table>,
    pub binding: [u8; 32],
}

impl Catalog {
    /// A read-only table catalog; no CDC catalog, marker or administrative grant.
    pub async fn read_snapshot(client: &mut SqlClient, config: &NativeConfig) -> Result<Self> {
        ensure!(
            !config.connection.tables.is_empty(),
            "read-only SQL Server snapshot requires explicit tables"
        );
        let mut keys = PrimaryKeyCache::new();
        keys.discover_keys(client, &config.connection).await?;
        let mut tables = Vec::new();
        for selected in &config.connection.tables {
            let (schema, name) = table_name(selected)?;
            let rows = client.query(
                "SELECT s.name,t.name,t.object_id FROM sys.tables t JOIN sys.schemas s ON s.schema_id=t.schema_id \
                 WHERE s.name=@P1 AND t.name=@P2",&[&schema,&name]).await?.into_first_result().await?;
            let [row] = rows.as_slice() else {
                anyhow::bail!("SQL Server snapshot table {selected} is missing or invisible");
            };
            let schema = row
                .try_get::<&str, _>(0)?
                .context("missing snapshot schema")?
                .to_owned();
            let name = row
                .try_get::<&str, _>(1)?
                .context("missing snapshot table")?
                .to_owned();
            let object: i32 = row.try_get(2)?.context("missing snapshot table object")?;
            let columns = client
                .query(
                    "SELECT c.name,ty.name,c.max_length,c.precision,c.scale,c.is_nullable \
                 FROM sys.columns c JOIN sys.types ty ON ty.user_type_id=c.user_type_id \
                 WHERE c.object_id=@P1 ORDER BY c.column_id",
                    &[&object],
                )
                .await?
                .into_first_result()
                .await?;
            let mut mapped: Vec<(String, String, i16, u8, u8, bool)> = Vec::new();
            for column in columns {
                mapped.push((
                    column
                        .try_get::<&str, _>(0)?
                        .context("missing snapshot column")?
                        .into(),
                    column
                        .try_get::<&str, _>(1)?
                        .context("missing snapshot type")?
                        .into(),
                    column.try_get(2)?.context("missing width")?,
                    column.try_get(3)?.context("missing precision")?,
                    column.try_get(4)?.context("missing scale")?,
                    column.try_get(5)?.context("missing nullability")?,
                ));
            }
            let table_keys = keys
                .get(&format!("{schema}.{name}"))
                .cloned()
                .context("snapshot requires primary/custom keys")?;
            ensure!(
                !mapped.is_empty()
                    && mapped
                        .iter()
                        .all(|column| property_type(&column.1).is_some()),
                "unsupported SQL Server snapshot column type"
            );
            ensure!(
                table_keys
                    .iter()
                    .all(|key| mapped.iter().any(|column| column.0 == *key)),
                "snapshot key column is missing"
            );
            tables.push(Table {
                schema,
                name,
                object,
                capture_object: 0,
                capture: String::new(),
                capture_start: Vec::new(),
                columns: mapped,
                keys: table_keys,
            });
        }
        let bytes = serde_json::to_vec(&tables)?;
        Ok(Self {
            tables,
            binding: Sha256::digest(bytes).into(),
        })
    }

    pub async fn read(client: &mut SqlClient, config: &NativeConfig) -> Result<Self> {
        let rows = client.query(
            "SELECT s.name, t.name, t.object_id, ct.object_id, ct.capture_instance, ct.start_lsn \
             FROM cdc.change_tables ct JOIN sys.tables t ON t.object_id=ct.source_object_id \
             JOIN sys.schemas s ON s.schema_id=t.schema_id ORDER BY s.name,t.name,ct.capture_instance", &[])
            .await?.into_first_result().await?;
        let selected = config
            .connection
            .tables
            .iter()
            .map(|t| table_name(t))
            .collect::<Result<Vec<_>>>()?;
        let barrier = config
            .barrier_table
            .as_deref()
            .map(table_name)
            .transpose()?;
        let mut keys = PrimaryKeyCache::new();
        keys.discover_keys(client, &config.connection).await?;
        let mut tables = Vec::new();
        let mut found = BTreeSet::new();
        for row in rows {
            let schema: &str = row.try_get(0)?.context("missing SQL Server table schema")?;
            let name: &str = row.try_get(1)?.context("missing SQL Server table name")?;
            if barrier.as_ref().is_some_and(|(s, n)| {
                s.eq_ignore_ascii_case(schema) && n.eq_ignore_ascii_case(name)
            }) {
                continue;
            }
            if !selected.is_empty()
                && !selected
                    .iter()
                    .any(|(s, n)| s.eq_ignore_ascii_case(schema) && n.eq_ignore_ascii_case(name))
            {
                continue;
            }
            let object: i32 = row.try_get(2)?.context("missing SQL Server table object")?;
            ensure!(found.insert(object), "multiple CDC capture instances for {schema}.{name}; select one generation before activation");
            let capture_object: i32 = row.try_get(3)?.context("missing CDC capture object")?;
            let capture: &str = row.try_get(4)?.context("missing CDC capture instance")?;
            let capture_start: &[u8] = row.try_get(5)?.unwrap_or(&[]);
            let canonical = format!("{schema}.{name}");
            let table_keys = keys
                .get(&canonical)
                .cloned()
                .context(format!("no primary/custom key for {canonical}"))?;
            let columns = client.query(
                "SELECT cc.name, ty.name, cc.max_length, cc.precision, cc.scale, sc.is_nullable, \
                 CASE WHEN sc.system_type_id=cc.system_type_id AND sc.max_length=cc.max_length \
                 AND sc.precision=cc.precision AND sc.scale=cc.scale THEN 1 ELSE 0 END \
                 FROM sys.columns cc JOIN sys.types ty ON ty.user_type_id=cc.user_type_id \
                 LEFT JOIN sys.columns sc ON sc.object_id=@P1 AND sc.name=cc.name \
                 WHERE cc.object_id=@P2 AND LEFT(cc.name,3)<>'__$' ORDER BY cc.column_id",
                &[&object, &capture_object]).await?.into_first_result().await?;
            let mut mapped: Vec<(String, String, i16, u8, u8, bool)> = Vec::new();
            for col in columns {
                let name: &str = col.try_get(0)?.context("missing CDC column")?;
                ensure!(col.try_get::<i32,_>(6)? == Some(1), "SQL Server captured column {canonical}.{name} changed; explicit recapture/reset required");
                mapped.push((
                    name.into(),
                    col.try_get::<&str, _>(1)?
                        .context("missing CDC column type")?
                        .into(),
                    col.try_get(2)?.context("missing CDC width")?,
                    col.try_get(3)?.context("missing CDC precision")?,
                    col.try_get(4)?.context("missing CDC scale")?,
                    col.try_get(5)?.context("missing CDC nullability")?,
                ));
            }
            ensure!(
                !mapped.is_empty()
                    && table_keys
                        .iter()
                        .all(|key| mapped.iter().any(|c| c.0 == *key)),
                "SQL Server CDC must capture every primary/custom key column"
            );
            ensure!(
                mapped.iter().all(|c| property_type(&c.1).is_some()),
                "unsupported SQL Server CDC column type in {canonical}"
            );
            tables.push(Table {
                schema: schema.into(),
                name: name.into(),
                object,
                capture_object,
                capture: capture.into(),
                capture_start: capture_start.to_vec(),
                columns: mapped,
                keys: table_keys,
            });
        }
        ensure!(
            !tables.is_empty() && tables.len() <= 256,
            "SQL Server selection has no CDC-enabled tables or exceeds limit"
        );
        ensure!(
            selected.is_empty() || tables.len() == selected.len(),
            "one or more selected SQL Server tables are absent or not CDC-enabled"
        );
        let identity = client.query("SELECT CONVERT(nvarchar(36),service_broker_guid) FROM sys.databases WHERE name=DB_NAME()", &[])
            .await?.into_first_result().await?;
        let identity: &str = identity
            .first()
            .context("SQL Server database identity missing")?
            .try_get(0)?
            .context("SQL Server database identity is null")?;
        let bytes = serde_json::to_vec(&(
            identity,
            &config.connection.host,
            config.connection.port,
            &config.connection.database,
            &tables,
        ))?;
        Ok(Self {
            tables,
            binding: Sha256::digest(bytes).into(),
        })
    }
    pub fn schema(&self) -> SourceSchema {
        SourceSchema {
            nodes: self
                .tables
                .iter()
                .map(|table| NodeSchema {
                    label: table.name.clone(),
                    properties: table
                        .columns
                        .iter()
                        .map(|column| PropertySchema {
                            name: column.0.clone(),
                            data_type: property_type(&column.1),
                            description: None,
                        })
                        .collect(),
                })
                .collect(),
            relations: Vec::new(),
        }
    }
}

fn property_type(name: &str) -> Option<PropertyType> {
    match name {
        "tinyint" | "smallint" | "int" | "bigint" => Some(PropertyType::Integer),
        "decimal" | "numeric" | "float" | "real" | "money" | "smallmoney" => {
            Some(PropertyType::Float)
        }
        "bit" => Some(PropertyType::Boolean),
        "datetime" | "datetime2" | "datetimeoffset" | "smalldatetime" => {
            Some(PropertyType::Timestamp)
        }
        "date" | "time" | "char" | "nchar" | "varchar" | "nvarchar" | "text" | "ntext"
        | "uniqueidentifier" | "binary" | "varbinary" | "image" | "timestamp" => {
            Some(PropertyType::String)
        }
        _ => None,
    }
}

pub async fn describe_schema(config: &NativeConfig) -> Result<SourceSchema> {
    let mut connection = MsSqlConnection::connect(&config.connection).await?;
    Ok(Catalog::read(connection.client_mut(), config)
        .await?
        .schema())
}
