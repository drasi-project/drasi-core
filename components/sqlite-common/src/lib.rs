// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use base64::Engine;
use drasi_core_types::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};
use rusqlite::{types::ValueRef, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// A parameter value for parameterized SQL execution.
#[derive(Debug, Clone)]
pub enum SqliteParam {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Bool(bool),
}

/// Runtime configuration for the optional SQLite REST API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestApiConfig {
    pub host: String,
    pub port: u16,
}

impl RestApiConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.host.is_empty() && !self.host.contains('\0'),
            "SQLite REST host must be a nonempty address"
        );
        Ok(())
    }
}

impl Default for RestApiConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
        }
    }
}

#[derive(Clone)]
pub struct TableSchema {
    pub columns: Vec<String>,
    pub keys: Vec<String>,
    pub rowid_alias: Option<&'static str>,
}

pub fn validate_selection(tables: &[String], table_keys: &[TableKeyConfig]) -> Result<()> {
    let valid_table = |table: &str| {
        !table.is_empty()
            && !table.contains('\0')
            && !table.to_ascii_lowercase().starts_with("__drasi_")
    };
    anyhow::ensure!(
        tables.iter().all(|table| valid_table(table)),
        "invalid native SQLite table name"
    );
    let unique: BTreeSet<_> = tables.iter().collect();
    anyhow::ensure!(
        unique.len() == tables.len(),
        "duplicate native SQLite table"
    );
    let mut keyed_tables = BTreeSet::new();
    for key in table_keys {
        anyhow::ensure!(
            valid_table(&key.table) && keyed_tables.insert(key.table.clone()),
            "invalid or duplicate native SQLite key table"
        );
        anyhow::ensure!(
            tables.is_empty() || tables.contains(&key.table),
            "native SQLite key configuration names an unselected table"
        );
        let mut columns = BTreeSet::new();
        anyhow::ensure!(
            !key.key_columns.is_empty()
                && key.key_columns.iter().all(|column| {
                    !column.is_empty()
                        && !column.contains('\0')
                        && columns.insert(column.to_ascii_lowercase())
                }),
            "native SQLite key columns must be nonempty, distinct identifiers"
        );
    }
    Ok(())
}

pub fn read_table_schemas(
    connection: &Connection,
    selected: &[String],
    overrides: &[TableKeyConfig],
    max_tables: usize,
    max_bytes: usize,
) -> Result<BTreeMap<String, TableSchema>> {
    validate_selection(selected, overrides)?;
    anyhow::ensure!(
        max_tables > 0 && max_bytes > 0,
        "SQLite schema limits must be positive"
    );
    let mut tables = BTreeMap::new();
    let mut listing = connection.prepare(
        "SELECT name, type FROM pragma_table_list WHERE schema='main' AND lower(substr(name,1,7)) != 'sqlite_' AND lower(substr(name,1,8)) != '__drasi_' ORDER BY name",
    )?;
    let mut rows = listing.query([])?;
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        let kind: String = row.get(1)?;
        if kind == "view" {
            continue;
        }
        anyhow::ensure!(
            kind == "table",
            "native SQLite capture requires ordinary tables"
        );
        let mut foreign_keys =
            connection.prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1)")?;
        let mut targets = foreign_keys.query([&name])?;
        while let Some(target) = targets.next()? {
            anyhow::ensure!(
                !target
                    .get::<_, String>(0)?
                    .to_ascii_lowercase()
                    .starts_with("__drasi_"),
                "native SQLite application tables cannot reference journal tables"
            );
        }
        let mut info = connection
            .prepare("SELECT name, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid")?;
        let mut columns = info.query([&name])?;
        let mut table = TableSchema {
            columns: Vec::new(),
            keys: Vec::new(),
            rowid_alias: None,
        };
        while let Some(column) = columns.next()? {
            let column_name: String = column.get(0)?;
            let hidden: i64 = column.get(2)?;
            anyhow::ensure!(
                hidden == 0,
                "native SQLite capture does not support generated/hidden columns"
            );
            bytes = bytes
                .checked_add(name.len() + column_name.len())
                .context("SQLite schema byte overflow")?;
            anyhow::ensure!(bytes <= max_bytes, "SQLite schema exceeds byte limit");
            if column.get::<_, i64>(1)? > 0 {
                table.keys.push(column_name.clone());
            }
            table.columns.push(column_name);
        }
        if let Some(configured) = overrides.iter().find(|key| key.table == name) {
            table.keys = configured
                .key_columns
                .iter()
                .map(|key| {
                    table
                        .columns
                        .iter()
                        .find(|column| column.eq_ignore_ascii_case(key))
                        .cloned()
                        .with_context(|| {
                            format!(
                                "native SQLite key column '{key}' is missing from table '{name}'"
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?;
        }
        if table.keys.is_empty() {
            table.rowid_alias = Some(
                ["_rowid_", "rowid", "oid"].into_iter()
                    .find(|alias| table.columns.iter().all(|column| !column.eq_ignore_ascii_case(alias)))
                    .with_context(|| format!(
                        "native SQLite table '{name}' shadows every rowid alias; configure explicit key columns"
                    ))?,
            );
        }
        anyhow::ensure!(
            tables.len() < max_tables,
            "SQLite schema table count exceeds limit"
        );
        tables.insert(name, table);
    }
    Ok(tables)
}

pub fn value_to_json(value: ValueRef<'_>) -> Result<serde_json::Value> {
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

pub fn row_element<'a>(
    source: &str,
    table_name: &str,
    table: &TableSchema,
    time: u64,
    rowid: Option<i64>,
    count: i32,
    mut value: impl FnMut(i32) -> rusqlite::Result<ValueRef<'a>>,
) -> Result<Element> {
    anyhow::ensure!(
        usize::try_from(count)? == table.columns.len(),
        "SQLite capture schema changed"
    );
    anyhow::ensure!(
        !table.keys.is_empty() || table.rowid_alias.is_some(),
        "native SQLite row identity is unavailable"
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
        let value = value_to_json(raw)?;
        if table.keys.contains(name) {
            anyhow::ensure!(!value.is_null(), "native SQLite element key cannot be null");
            keys.insert(name, (kind, value.clone()));
        }
        properties.insert(name, (&value).into());
    }
    let id = if table.rowid_alias.is_some() {
        format!(
            "sqlite:rowid:v1:{}",
            serde_json::to_string(&(
                table_name,
                rowid.context("native SQLite rowid is unavailable")?,
            ))?
        )
    } else {
        format!("sqlite:v1:{}", serde_json::to_string(&(table_name, keys))?)
    };
    Ok(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, &id),
            labels: Arc::from([Arc::from(table_name)]),
            effective_from: time,
        },
        properties,
    })
}

pub fn read_table(
    connection: &Connection,
    source: &str,
    name: &str,
    table: &TableSchema,
    time: u64,
    mut emit: impl FnMut(Element) -> Result<ControlFlow<()>>,
) -> Result<ControlFlow<()>> {
    let offset = usize::from(table.rowid_alias.is_some());
    let columns = table
        .rowid_alias
        .map(|alias| format!("\"{alias}\",*"))
        .unwrap_or_else(|| "*".into());
    let mut statement = connection.prepare(&format!(
        "SELECT {columns} FROM \"{}\"",
        name.replace('"', "\"\""),
    ))?;
    let count = i32::try_from(
        statement
            .column_count()
            .checked_sub(offset)
            .context("SQLite snapshot column count is invalid")?,
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let element = row_element(
            source,
            name,
            table,
            time,
            if offset == 0 { None } else { Some(row.get(0)?) },
            count,
            |index| row.get_ref(index as usize + offset),
        )?;
        if let ControlFlow::Break(()) = emit(element)? {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}
