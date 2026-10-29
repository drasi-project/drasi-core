// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{read_table_schemas, TableKeyConfig};
use anyhow::{Context, Result};
use drasi_core_types::schema::{NodeSchema, PropertySchema, PropertyType, SourceSchema};
use rusqlite::Connection;

fn hint(declared: &str) -> Option<PropertyType> {
    let declared = declared.to_ascii_uppercase();
    if declared.contains("INT") {
        Some(PropertyType::Integer)
    } else if ["CHAR", "CLOB", "TEXT"]
        .iter()
        .any(|kind| declared.contains(kind))
    {
        Some(PropertyType::String)
    } else if ["REAL", "FLOA", "DOUB"]
        .iter()
        .any(|kind| declared.contains(kind))
    {
        Some(PropertyType::Float)
    } else {
        None
    }
}

/// Report actual captured columns, with conservative SQLite affinity hints.
/// Dynamic/numeric/blob affinities do not imply a fixed graph property type.
pub fn describe_source(
    connection: &Connection,
    selected: &[String],
    overrides: &[TableKeyConfig],
    max_tables: usize,
    max_bytes: usize,
) -> Result<SourceSchema> {
    let tables = read_table_schemas(connection, selected, overrides, max_tables, max_bytes)?;
    let mut result = SourceSchema::default();
    let mut bytes = serde_json::to_vec(&result)?.len();
    anyhow::ensure!(
        bytes <= max_bytes,
        "SQLite source schema exceeds byte limit"
    );
    for (name, captured) in tables {
        let mut node = NodeSchema::new(name.clone());
        bytes = bytes
            .checked_add(serde_json::to_vec(&node)?.len())
            .and_then(|bytes| bytes.checked_add(usize::from(!result.nodes.is_empty())))
            .context("SQLite source schema size overflow")?;
        anyhow::ensure!(
            bytes <= max_bytes,
            "SQLite source schema exceeds byte limit"
        );
        let mut statement = connection.prepare(
            "SELECT name,type,\"notnull\",pk,hidden FROM pragma_table_xinfo(?1) ORDER BY cid",
        )?;
        let mut rows = statement.query([&name])?;
        while let Some(row) = rows.next()? {
            let column = row.get_ref(0)?.as_str()?;
            let declared = row.get_ref(1)?.as_str()?;
            anyhow::ensure!(
                column.len() <= max_bytes && declared.len() <= max_bytes,
                "SQLite source schema exceeds byte limit"
            );
            anyhow::ensure!(
                row.get::<_, i64>(4)? == 0
                    && captured
                        .columns
                        .get(node.properties.len())
                        .is_some_and(|name| name == column),
                "SQLite discovery schema differs from captured columns"
            );
            let mut description = if declared.is_empty() {
                "SQLite column without a declared type".to_owned()
            } else {
                format!("SQLite declared type: {declared}")
            };
            if row.get::<_, i64>(2)? != 0 {
                description.push_str("; NOT NULL");
            }
            if row.get::<_, i64>(3)? != 0 {
                description.push_str("; primary key");
            }
            if captured.keys.iter().any(|key| key == column) {
                description.push_str("; element key");
            }
            let property = PropertySchema {
                name: column.to_owned(),
                data_type: hint(declared),
                description: Some(description),
            };
            bytes = bytes
                .checked_add(serde_json::to_vec(&property)?.len())
                .and_then(|bytes| bytes.checked_add(usize::from(!node.properties.is_empty())))
                .context("SQLite source schema size overflow")?;
            anyhow::ensure!(
                bytes <= max_bytes,
                "SQLite source schema exceeds byte limit"
            );
            node.properties.push(property);
        }
        anyhow::ensure!(
            node.properties.len() == captured.columns.len(),
            "SQLite discovery schema changed"
        );
        result.nodes.push(node);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_columns_keys_and_exact_serialized_bound() -> Result<()> {
        let database = Connection::open_in_memory()?;
        database.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY,code TEXT NOT NULL,value REAL,raw BLOB,ambiguous NUMERIC);
             CREATE TABLE ignored(id INTEGER);
             CREATE TABLE __drasi_internal(id INTEGER);",
        )?;
        let keys = [TableKeyConfig {
            table: "items".into(),
            key_columns: vec!["code".into()],
        }];
        let result = describe_source(&database, &["items".into()], &keys, 4, 65536)?;
        assert_eq!(result.nodes.len(), 1);
        assert!(result.relations.is_empty());
        let node = &result.nodes[0];
        assert_eq!(node.label, "items");
        assert_eq!(
            node.properties
                .iter()
                .map(|property| property.data_type)
                .collect::<Vec<_>>(),
            [
                Some(PropertyType::Integer),
                Some(PropertyType::String),
                Some(PropertyType::Float),
                None,
                None
            ]
        );
        assert!(node.properties[1]
            .description
            .as_ref()
            .is_some_and(|value| value.contains("NOT NULL") && value.contains("element key")));
        let size = serde_json::to_vec(&result)?.len();
        assert_eq!(
            describe_source(&database, &["items".into()], &keys, 4, size)?,
            result
        );
        assert!(describe_source(&database, &["items".into()], &keys, 4, size - 1).is_err());
        let all = describe_source(&database, &[], &[], 4, 65536)?;
        assert_eq!(
            all.nodes
                .iter()
                .map(|node| node.label.as_str())
                .collect::<Vec<_>>(),
            ["ignored", "items"]
        );
        assert!(describe_source(&database, &[], &[], 1, 65536).is_err());
        Ok(())
    }

    #[test]
    fn rowid_identity_does_not_invent_an_emitted_property() -> Result<()> {
        let database = Connection::open_in_memory()?;
        database.execute_batch("CREATE TABLE keyless(value TEXT, rowid INTEGER)")?;
        let result = describe_source(&database, &[], &[], 2, 65536)?;
        assert_eq!(
            result.nodes[0]
                .properties
                .iter()
                .map(|property| property.name.as_str())
                .collect::<Vec<_>>(),
            ["value", "rowid"]
        );
        assert!(result.nodes[0].properties.iter().all(|property| !property
            .description
            .as_ref()
            .expect("description")
            .contains("element key")));
        Ok(())
    }
}
