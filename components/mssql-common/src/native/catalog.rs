// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The selected tables, their capture instances, keys and value mapping, and
//! the binding a committed cursor depends on.
//!
//! Decision D-7: the binding covers only the facts a cursor relies on — the
//! database identity, each selected table's object, capture instance and key
//! columns with their types. Host, port and non-key columns are not bound,
//! so a DNS or availability-group listener rename keeps every cursor, and a
//! non-key column change does not invalidate one.

use super::{
    error::{schema_changed, unsupported},
    identifiers::{quoted, TableName},
    session::{CaptureInstance, CdcSession, DatabaseIdentity, RawColumn, TableFacts},
    values::ColumnKind,
    NativeSettings,
};
use anyhow::{ensure, Result};
use drasi_core_types::schema::{NodeSchema, PropertySchema, SourceSchema};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// One captured column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub kind: ColumnKind,
}

/// One selected table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub object: i32,
    /// The change table's object (0 for a read-only snapshot).
    pub capture_object: i32,
    /// The capture instance (empty for a read-only snapshot).
    pub capture: String,
    pub columns: Vec<Column>,
    pub keys: Vec<String>,
    /// Base types of the key columns in the source table.
    pub key_types: Vec<String>,
}

impl Table {
    /// `schema.table`, the element ID prefix.
    pub fn canonical(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
    /// `[schema].[table]`.
    pub fn sql(&self) -> String {
        format!("{}.{}", quoted(&self.schema), quoted(&self.name))
    }
    /// `cdc.[capture_CT]`.
    pub fn change_table(&self) -> String {
        format!("cdc.{}", quoted(&format!("{}_CT", self.capture)))
    }
    /// The SELECT list with every value conversion.
    pub fn selection(&self) -> String {
        self.columns
            .iter()
            .map(|column| column.kind.select(&column.name, &column.type_name))
            .collect::<Vec<_>>()
            .join(", ")
    }
    /// Columns that are streamed as text or NULL without a schema type.
    pub fn unmapped(&self) -> Vec<&Column> {
        self.columns
            .iter()
            .filter(|column| column.kind.property_type().is_none())
            .collect()
    }
}

/// The selected tables and their binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub tables: Vec<Table>,
    pub binding: [u8; 32],
}

#[derive(Serialize)]
struct BoundTable<'a> {
    schema: String,
    name: String,
    object: i32,
    capture_object: i32,
    capture: &'a str,
    keys: Vec<String>,
    key_types: Vec<String>,
}

/// The SHA-256 binding over the facts a cursor depends on.
pub fn binding(database: &DatabaseIdentity, tables: &[Table]) -> Result<[u8; 32]> {
    let mut bound: Vec<BoundTable<'_>> = tables
        .iter()
        .map(|table| BoundTable {
            schema: table.schema.to_lowercase(),
            name: table.name.to_lowercase(),
            object: table.object,
            capture_object: table.capture_object,
            capture: &table.capture,
            keys: table.keys.iter().map(|key| key.to_lowercase()).collect(),
            key_types: table.key_types.iter().map(|t| t.to_lowercase()).collect(),
        })
        .collect();
    bound.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
    let mut hash = Sha256::new();
    hash.update(b"drasi.mssql/binding/v2\0");
    hash.update(database.guid.to_lowercase().as_bytes());
    hash.update(b"\0");
    hash.update(database.name.to_lowercase().as_bytes());
    hash.update(b"\0");
    hash.update(serde_json::to_vec(&bound)?);
    Ok(hash.finalize().into())
}

fn column_type<'a>(columns: &'a [RawColumn], name: &str) -> Option<&'a RawColumn> {
    columns
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(name))
}

/// Builds one table from its catalog facts: resolves keys (explicit
/// `tableKeys` first), classifies columns and checks that every key column
/// is captured and keyable.
pub fn build_table(
    settings: &NativeSettings,
    capture: &CaptureInstance,
    facts: TableFacts,
) -> Result<Table> {
    let name = TableName {
        schema: capture.schema.clone(),
        table: capture.table.clone(),
    };
    let canonical = format!("{}.{}", capture.schema, capture.table);
    let keys = match settings.explicit_key(&name)? {
        Some(keys) => keys,
        None if !facts.primary_key.is_empty() => facts.primary_key.clone(),
        None => {
            return Err(unsupported(format!(
                "{canonical} has no primary key; add a tableKeys entry"
            )))
        }
    };
    let columns: Vec<Column> = facts
        .columns
        .iter()
        .map(|column| Column {
            name: column.name.clone(),
            type_name: column.type_name.clone(),
            kind: ColumnKind::of(&column.type_name, column.precision),
        })
        .collect();
    ensure!(!columns.is_empty(), "{canonical} has no readable columns");
    let mut resolved = Vec::new();
    let mut key_types = Vec::new();
    for key in &keys {
        let Some(column) = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(key))
        else {
            return Err(unsupported(format!(
                "key column {canonical}.{key} is not captured; the capture instance must include every key column"
            )));
        };
        if !column.kind.keyable() {
            return Err(unsupported(format!(
                "key column {canonical}.{key} has type {}, which cannot form an element key",
                column.type_name
            )));
        }
        resolved.push(column.name.clone());
        key_types.push(
            column_type(&facts.source_columns, key)
                .map_or_else(|| column.type_name.clone(), |c| c.type_name.clone()),
        );
    }
    Ok(Table {
        schema: capture.schema.clone(),
        name: capture.table.clone(),
        object: capture.object,
        capture_object: capture.capture_object,
        capture: capture.capture.clone(),
        columns,
        keys: resolved,
        key_types,
    })
}

/// The capture instances the settings select, in catalog order. Explicit
/// tables must each have exactly one capture instance; with no explicit
/// tables, every captured table except the barrier is selected.
pub fn select_captures(
    settings: &NativeSettings,
    captures: &[CaptureInstance],
) -> Result<Vec<CaptureInstance>> {
    let selected = settings.selected_tables()?;
    let barrier = settings.barrier()?;
    let mut chosen: Vec<CaptureInstance> = Vec::new();
    for capture in captures {
        if barrier
            .as_ref()
            .is_some_and(|barrier| barrier.matches(&capture.schema, &capture.table))
        {
            continue;
        }
        if !selected.is_empty()
            && !selected
                .iter()
                .any(|table| table.matches(&capture.schema, &capture.table))
        {
            continue;
        }
        if chosen.iter().any(|other| other.object == capture.object) {
            return Err(schema_changed(format!(
                "{}.{} has more than one CDC capture instance; drop the old instance before (re)starting the source",
                capture.schema, capture.table
            )));
        }
        chosen.push(capture.clone());
    }
    for table in &selected {
        ensure!(
            chosen
                .iter()
                .any(|capture| table.matches(&capture.schema, &capture.table)),
            "selected table {}.{} is missing or not CDC-enabled (or the login cannot see it)",
            table.schema,
            table.table
        );
    }
    ensure!(
        !chosen.is_empty(),
        "no CDC-enabled table is selected; enable CDC on a table or fix tables"
    );
    ensure!(
        chosen.len() <= super::settings::MAX_TABLES,
        "more than {} CDC tables are selected; list tables explicitly",
        super::settings::MAX_TABLES
    );
    Ok(chosen)
}

/// Reads the CDC catalog of the selected tables. With `check_permissions`,
/// fails with an actionable message when the login cannot read a change
/// table (MS-07).
pub async fn read_catalog(
    session: &mut dyn CdcSession,
    settings: &NativeSettings,
    check_permissions: bool,
) -> Result<Catalog> {
    let database = session.database().await?;
    let captures = select_captures(settings, &session.capture_instances().await?)?;
    let mut tables = Vec::new();
    for capture in &captures {
        let facts = session
            .table_facts(capture.object, Some(capture.capture_object))
            .await?;
        let table = build_table(settings, capture, facts)?;
        if check_permissions && !session.can_select(&table.change_table()).await? {
            return Err(unsupported(format!(
                "the login cannot read {} (change table of {}); grant SELECT on it, for example \
                 GRANT SELECT ON {} TO <login user>, or use a member of the capture instance's \
                 gating role with SELECT on the change table",
                table.change_table(),
                table.canonical(),
                table.change_table()
            )));
        }
        tables.push(table);
    }
    Ok(Catalog {
        binding: binding(&database, &tables)?,
        tables,
    })
}

/// Reads the explicitly selected tables for a read-only snapshot: no CDC
/// catalog, marker or administrative grant.
pub async fn read_snapshot_catalog(
    session: &mut dyn CdcSession,
    settings: &NativeSettings,
) -> Result<Catalog> {
    let selected = settings.selected_tables()?;
    ensure!(
        !selected.is_empty(),
        "a read-only SQL Server snapshot requires explicit tables"
    );
    let database = session.database().await?;
    let mut tables = Vec::new();
    for name in &selected {
        let Some((object, schema, table)) = session.find_table(&name.schema, &name.table).await?
        else {
            anyhow::bail!(
                "snapshot table {}.{} is missing or invisible to the login",
                name.schema,
                name.table
            );
        };
        let facts = session.table_facts(object, None).await?;
        let capture = CaptureInstance {
            schema,
            table,
            object,
            capture_object: 0,
            capture: String::new(),
        };
        tables.push(build_table(settings, &capture, facts)?);
    }
    Ok(Catalog {
        binding: binding(&database, &tables)?,
        tables,
    })
}

impl Catalog {
    /// The published schema: one node label per table, as emitted.
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
                            name: column.name.clone(),
                            data_type: column.kind.property_type(),
                            description: None,
                        })
                        .collect(),
                })
                .collect(),
            relations: Vec::new(),
        }
    }

    /// The table with `object`, if selected.
    pub fn table(&self, object: i32) -> Option<&Table> {
        self.tables.iter().find(|table| table.object == object)
    }
}

/// Reads the selected tables' schema with one short-lived session.
pub async fn describe_schema(
    session: &mut dyn CdcSession,
    settings: &NativeSettings,
) -> Result<SourceSchema> {
    Ok(read_catalog(session, settings, false).await?.schema())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use drasi_computation_contracts::source_failure;

    pub(crate) fn raw(name: &str, type_name: &str) -> RawColumn {
        RawColumn {
            name: name.into(),
            type_name: type_name.into(),
            precision: 10,
            scale: 0,
        }
    }

    pub(crate) fn capture(table: &str, object: i32, capture: &str) -> CaptureInstance {
        CaptureInstance {
            schema: "dbo".into(),
            table: table.into(),
            object,
            capture_object: object + 1000,
            capture: capture.into(),
        }
    }

    pub(crate) fn facts() -> TableFacts {
        TableFacts {
            columns: vec![raw("id", "int"), raw("value", "int"), raw("doc", "xml")],
            source_columns: vec![raw("id", "int"), raw("value", "int"), raw("doc", "xml")],
            primary_key: vec!["id".into()],
        }
    }

    fn database() -> DatabaseIdentity {
        DatabaseIdentity {
            guid: "8D3E...".into(),
            name: "app".into(),
        }
    }

    fn settings() -> NativeSettings {
        NativeSettings::new("app")
    }

    #[test]
    fn binding_covers_keys_and_capture_but_not_non_key_columns_or_host() {
        let table = build_table(&settings(), &capture("Items", 7, "dbo_Items"), facts()).unwrap();
        let base = binding(&database(), std::slice::from_ref(&table)).unwrap();
        let mut moved = settings();
        moved.host = "sqlag-listener.corp".into();
        moved.port = 14330;
        let same = build_table(&moved, &capture("Items", 7, "dbo_Items"), facts()).unwrap();
        assert_eq!(
            binding(&database(), &[same]).unwrap(),
            base,
            "host rename keeps cursors"
        );
        let mut wider = facts();
        wider.columns.push(raw("note", "nvarchar"));
        let wider = build_table(&settings(), &capture("Items", 7, "dbo_Items"), wider).unwrap();
        assert_eq!(
            binding(&database(), &[wider]).unwrap(),
            base,
            "non-key column"
        );
        let recaptured =
            build_table(&settings(), &capture("Items", 7, "dbo_Items_v2"), facts()).unwrap();
        assert_ne!(binding(&database(), &[recaptured]).unwrap(), base);
        let mut retyped = facts();
        retyped.source_columns[0] = raw("id", "bigint");
        let retyped = build_table(&settings(), &capture("Items", 7, "dbo_Items"), retyped).unwrap();
        assert_ne!(binding(&database(), &[retyped]).unwrap(), base);
        let mut other = database();
        other.guid = "other".into();
        assert_ne!(binding(&other, std::slice::from_ref(&table)).unwrap(), base);
    }

    #[test]
    fn unmapped_columns_stream_without_rejecting_the_table() {
        let table = build_table(&settings(), &capture("Items", 7, "dbo_Items"), facts()).unwrap();
        assert_eq!(table.unmapped().len(), 1);
        assert!(table
            .selection()
            .contains("CONVERT(nvarchar(max), [doc]) AS [doc]"));
        let catalog = Catalog {
            tables: vec![table],
            binding: [0; 32],
        };
        let schema = catalog.schema();
        assert_eq!(schema.nodes[0].label, "Items");
        assert_eq!(schema.nodes[0].properties[2].data_type, None);
    }

    #[test]
    fn keys_must_exist_be_captured_and_keyable() {
        let mut keyless = facts();
        keyless.primary_key.clear();
        let error =
            build_table(&settings(), &capture("Items", 7, "c"), keyless.clone()).unwrap_err();
        assert!(matches!(
            source_failure(&error),
            Some(drasi_computation_contracts::SourceFailure::Unsupported(_))
        ));
        let mut explicit = settings();
        explicit.table_keys = vec![super::super::settings::TableKey {
            table: "dbo.Items".into(),
            key_columns: vec!["value".into()],
        }];
        let table = build_table(&explicit, &capture("Items", 7, "c"), keyless).unwrap();
        assert_eq!(table.keys, ["value"]);
        let mut xml_key = facts();
        xml_key.primary_key = vec!["doc".into()];
        assert!(build_table(&settings(), &capture("Items", 7, "c"), xml_key).is_err());
        let mut uncaptured = facts();
        uncaptured.columns.remove(0);
        assert!(build_table(&settings(), &capture("Items", 7, "c"), uncaptured).is_err());
    }

    #[test]
    fn selection_ignores_unselected_tables_and_types_capture_changes() {
        let captures = vec![
            capture("Barrier", 1, "dbo_Barrier"),
            capture("Items", 7, "dbo_Items"),
            capture("Other", 9, "dbo_Other"),
        ];
        let mut explicit = settings();
        explicit.tables = vec!["Items".into()];
        explicit.barrier_table = Some("Barrier".into());
        let chosen = select_captures(&explicit, &captures).unwrap();
        assert_eq!(chosen.len(), 1);
        let mut all = settings();
        all.barrier_table = Some("dbo.Barrier".into());
        assert_eq!(select_captures(&all, &captures).unwrap().len(), 2);
        let mut doubled = captures.clone();
        doubled.push(capture("Items", 7, "dbo_Items_v2"));
        let error = select_captures(&explicit, &doubled).unwrap_err();
        assert!(matches!(
            source_failure(&error),
            Some(drasi_computation_contracts::SourceFailure::SchemaChanged(_))
        ));
        let mut doubled_other = captures.clone();
        doubled_other.push(capture("Other", 9, "dbo_Other_v2"));
        select_captures(&explicit, &doubled_other).expect("unselected tables are ignored");
        let mut missing = settings();
        missing.tables = vec!["Absent".into()];
        assert!(select_captures(&missing, &captures).is_err());
    }
}
