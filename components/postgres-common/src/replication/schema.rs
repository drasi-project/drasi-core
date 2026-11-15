// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Source-data schema discovery for native PostgreSQL CDC.
//!
//! Describes the node labels and properties the source emits: one label per
//! selected publication table, named as [`super::row_element`] names it, with
//! property types that match [`crate::PostgresValue::to_element_value`].

use super::*;
use drasi_core_types::schema::{NodeSchema, PropertySchema, PropertyType, SourceSchema};

/// The emitted label for a table: the bare name in `public`, otherwise
/// `schema.table`.
pub fn table_label(namespace: &str, table: &str) -> String {
    if namespace == "public" {
        table.to_owned()
    } else {
        format!("{namespace}.{table}")
    }
}

/// The property type the source emits for a column of PostgreSQL type
/// `typname`, or `None` when it has no single scalar type (arrays, and types
/// decoded as text without a declared mapping).
pub fn property_type(typname: &str) -> Option<PropertyType> {
    match typname {
        "int2" | "int4" | "int8" => Some(PropertyType::Integer),
        "float4" | "float8" | "numeric" => Some(PropertyType::Float),
        "bool" => Some(PropertyType::Boolean),
        "timestamp" | "timestamptz" => Some(PropertyType::Timestamp),
        // Dates, times and JSON are emitted as strings by the native decoder.
        "text" | "varchar" | "bpchar" | "char" | "name" | "uuid" | "bytea" | "date" | "time"
        | "json" | "jsonb" => Some(PropertyType::String),
        _ => None,
    }
}

/// Describes the selected publication tables. Connects with `config`'s
/// credentials and closes the connection before returning.
pub async fn describe_schema(config: &PostgresTransactionConfig) -> Result<SourceSchema> {
    let mut connection = connect_with_options(
        ConnectionOptions {
            host: &config.host,
            port: config.port,
            database: &config.database,
            user: &config.user,
            password: &config.password,
            ssl_mode: config.ssl_mode,
            tls_ca_pem: config.tls_ca_pem.as_deref(),
            max_protocol_bytes: config.max_protocol_bytes,
            timeout: config.timeout(),
        },
        false,
    )
    .await?;
    let publication = quote_literal(&config.publication);
    let rows = connection
        .query_rows(
            format!(
                "SELECT p.schemaname, p.tablename, a.attname, t.typname \
                 FROM pg_publication_tables p \
                 JOIN pg_namespace n ON n.nspname = p.schemaname \
                 JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = p.tablename \
                 JOIN pg_attribute a ON a.attrelid = c.oid \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 WHERE p.pubname = {publication} AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY p.schemaname, p.tablename, a.attnum"
            ),
            config.max_protocol_bytes,
        )
        .await?;
    let _ = connection.close().await;
    let selected: BTreeSet<_> = config
        .tables
        .iter()
        .map(|table| table_name(table))
        .collect();
    let mut nodes = BTreeMap::<(String, String), Vec<PropertySchema>>::new();
    for row in &rows {
        let table = (field(row, 0)?, field(row, 1)?);
        if !selected.is_empty() && !selected.contains(&table) {
            continue;
        }
        nodes.entry(table).or_default().push(PropertySchema {
            name: field(row, 2)?,
            data_type: property_type(&field(row, 3)?),
            description: None,
        });
    }
    Ok(SourceSchema {
        nodes: nodes
            .into_iter()
            .map(|((namespace, table), properties)| NodeSchema {
                label: table_label(&namespace, &table),
                properties,
            })
            .collect(),
        relations: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_types_match_emitted_elements() {
        assert_eq!(table_label("public", "person"), "person");
        assert_eq!(table_label("sales", "order"), "sales.order");
        assert_eq!(property_type("int8"), Some(PropertyType::Integer));
        assert_eq!(property_type("numeric"), Some(PropertyType::Float));
        assert_eq!(property_type("timestamptz"), Some(PropertyType::Timestamp));
        assert_eq!(property_type("jsonb"), Some(PropertyType::String));
        assert_eq!(property_type("date"), Some(PropertyType::String));
        assert_eq!(property_type("_int4"), None);
    }
}
