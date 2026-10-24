// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::catalog::Tables;
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::SourceSchemaReader;
use drasi_core_types::schema::{NodeSchema, PropertySchema, PropertyType, SourceSchema};
use std::sync::{Arc, RwLock, Weak};

#[derive(Default)]
struct State {
    schema: Option<SourceSchema>,
    retired: bool,
}

struct Reader(Weak<RwLock<State>>);

#[async_trait]
impl SourceSchemaReader for Reader {
    async fn describe_schema(&self) -> Result<Option<SourceSchema>> {
        let owner = self.0.upgrade().context("MySQL schema source is retired")?;
        let state = owner
            .read()
            .map_err(|_| anyhow::anyhow!("MySQL schema publication poisoned"))?;
        anyhow::ensure!(!state.retired, "MySQL schema source is retired");
        Ok(state.schema.clone())
    }
}

pub struct Publication {
    state: Arc<RwLock<State>>,
    reader: Arc<Reader>,
}

impl Default for Publication {
    fn default() -> Self {
        let state = Arc::new(RwLock::new(State::default()));
        Self {
            reader: Arc::new(Reader(Arc::downgrade(&state))),
            state,
        }
    }
}

impl Publication {
    pub fn reader(&self) -> Arc<dyn SourceSchemaReader> {
        self.reader.clone()
    }
    pub fn clear(&self) -> Result<()> {
        self.state
            .write()
            .map_err(|_| anyhow::anyhow!("MySQL schema publication poisoned"))?
            .schema = None;
        Ok(())
    }
    pub fn capture(&self, tables: &Tables) -> Result<()> {
        self.capture_with_layout(tables, false)
    }
    pub fn capture_positional(&self, tables: &Tables) -> Result<()> {
        self.capture_with_layout(tables, true)
    }
    fn capture_with_layout(&self, tables: &Tables, positional: bool) -> Result<()> {
        let schema = SourceSchema {
            nodes: tables
                .iter()
                .map(|(table, columns)| NodeSchema {
                    label: table.clone(),
                    properties: columns
                        .iter()
                        .enumerate()
                        .map(|(index, column)| {
                            let name = column.sql_type.split(['(', ' ']).next().unwrap_or_default();
                            let data_type = match name {
                                // Large unsigned integers are preserved as decimal strings.
                                "bigint"
                                    if column
                                        .sql_type
                                        .split_whitespace()
                                        .any(|part| part == "unsigned") =>
                                {
                                    None
                                }
                                "tinyint" | "smallint" | "mediumint" | "int" | "integer"
                                | "bigint" | "year" => Some(PropertyType::Integer),
                                "float" | "double" | "real" => Some(PropertyType::Float),
                                "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob"
                                | "longblob" | "bit" => Some(PropertyType::Json),
                                _ => Some(PropertyType::String),
                            };
                            PropertySchema {
                                name: if positional {
                                    format!("@{index}")
                                } else {
                                    column.name.clone()
                                },
                                data_type: if positional { None } else { data_type },
                                description: Some(format!(
                                    "MySQL {}{}; {}{}",
                                    if positional {
                                        format!("column {}: ", column.name)
                                    } else {
                                        String::new()
                                    },
                                    column.sql_type,
                                    if column.nullable {
                                        "nullable"
                                    } else {
                                        "not null"
                                    },
                                    if column.key { "; primary key" } else { "" }
                                )),
                            }
                        })
                        .collect(),
                })
                .collect(),
            relations: Vec::new(),
        };
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("MySQL schema publication poisoned"))?;
        anyhow::ensure!(!state.retired, "MySQL schema source is retired");
        state.schema = Some(schema);
        Ok(())
    }
}

impl Publication {
    pub fn retire(&self) {
        let mut state = self.state.write().unwrap_or_else(|error| {
            log::error!("Retiring poisoned MySQL schema publication");
            error.into_inner()
        });
        state.retired = true;
        state.schema = None;
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        self.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::catalog::Column;

    #[tokio::test]
    async fn captured_types_match_lossless_decoder_representations() -> Result<()> {
        let cases = [
            ("bigint unsigned", None),
            ("bigint", Some(PropertyType::Integer)),
            ("int unsigned", Some(PropertyType::Integer)),
            ("double", Some(PropertyType::Float)),
            ("decimal(30,10)", Some(PropertyType::String)),
            ("datetime(6)", Some(PropertyType::String)),
            ("json", Some(PropertyType::String)),
            ("varbinary(16)", Some(PropertyType::Json)),
            ("bit(8)", Some(PropertyType::Json)),
        ];
        let publication = Publication::default();
        publication.capture(&Tables::from([(
            "values".into(),
            cases
                .iter()
                .enumerate()
                .map(|(index, (sql_type, _))| Column {
                    name: format!("column_{index}"),
                    sql_type: (*sql_type).into(),
                    key: false,
                    nullable: true,
                    charset: None,
                    collation: None,
                })
                .collect(),
        )]))?;
        let schema = publication
            .reader()
            .describe_schema()
            .await?
            .context("captured type schema")?;
        for (property, (sql_type, expected)) in schema.nodes[0].properties.iter().zip(cases) {
            assert_eq!(property.data_type, expected, "{sql_type}");
            assert_eq!(
                property.description.as_deref(),
                Some(format!("MySQL {sql_type}; nullable").as_str())
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn schema_is_captured_not_configured_and_retained_readers_are_fenced() -> Result<()> {
        let publication = Publication::default();
        let reader = publication.reader();
        assert!(reader.describe_schema().await?.is_none());
        publication.capture(&Tables::from([(
            "items".into(),
            vec![
                Column {
                    name: "id".into(),
                    sql_type: "int".into(),
                    key: true,
                    nullable: false,
                    charset: None,
                    collation: None,
                },
                Column {
                    name: "payload".into(),
                    sql_type: "json".into(),
                    key: false,
                    nullable: true,
                    charset: None,
                    collation: None,
                },
            ],
        )]))?;
        let schema = reader.describe_schema().await?.context("captured schema")?;
        assert_eq!(schema.nodes[0].label, "items");
        assert_eq!(
            schema.nodes[0].properties[0].data_type,
            Some(PropertyType::Integer)
        );
        assert_eq!(
            schema.nodes[0].properties[1].data_type,
            Some(PropertyType::String)
        );
        assert!(schema.nodes[0].properties[0]
            .description
            .as_ref()
            .unwrap()
            .contains("primary key"));
        publication.clear()?;
        assert!(reader.describe_schema().await?.is_none());
        publication.retire();
        assert!(reader.describe_schema().await.is_err());
        assert!(publication.capture(&Tables::new()).is_err());
        drop(publication);
        assert!(reader.describe_schema().await.is_err());
        Ok(())
    }
}
