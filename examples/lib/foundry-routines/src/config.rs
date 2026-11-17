// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Connection {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password_ref: String,
    pub tls: String,
    pub tables: Vec<String>,
    #[serde(default)]
    pub table_keys: Vec<TableKey>,
    #[serde(default)]
    pub publication: String,
    #[serde(default)]
    pub slot: String,
    #[serde(default)]
    pub server_id: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKey {
    pub table: String,
    pub columns: Vec<String>,
}

impl Connection {
    pub fn validate_transport(&self) -> Result<()> {
        if !self.id.is_empty() {
            identifier(&self.id)?;
        }
        ensure!(
            ["postgres", "mysql"].contains(&self.kind.as_str()),
            "Unsupported source kind"
        );
        ensure!(
            !self.name.trim().is_empty() && !self.host.trim().is_empty(),
            "Name and host are required"
        );
        ensure!(
            self.port > 0 && !self.database.trim().is_empty() && !self.user.trim().is_empty(),
            "Port, database and user are required"
        );
        ensure!(
            self.password_ref.starts_with("FOUNDRY_DEMO_")
                && self
                    .password_ref
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
            "Credential references must be FOUNDRY_DEMO_ environment aliases"
        );
        ensure!(
            ["disabled", "verify"].contains(&self.tls.as_str()),
            "TLS must be disabled or verify"
        );
        ensure!(
            self.tls != "disabled"
                || ["localhost", "127.0.0.1", "::1"].contains(&self.host.as_str()),
            "Unencrypted connections are permitted only for local fixtures"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_transport()?;
        identifier(&self.id)?;
        ensure!(!self.tables.is_empty(), "Select at least one table");
        for table in &self.tables {
            qualified_identifier(table)?;
        }
        for key in &self.table_keys {
            ensure!(
                self.tables.contains(&key.table) && !key.columns.is_empty(),
                "Table keys must refer to selected tables"
            );
            for column in &key.columns {
                identifier(column)?;
            }
        }
        if self.kind == "postgres" {
            identifier(&self.slot)?;
            identifier(&self.publication)?;
        } else {
            ensure!(
                self.server_id > 0,
                "A unique nonzero MySQL server ID is required"
            );
        }
        Ok(())
    }

    pub fn password(&self) -> Result<String> {
        self.validate_transport()?;
        std::env::var(&self.password_ref).context("Database credential alias is not configured")
    }
}

pub fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 63
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "Invalid identifier"
    );
    Ok(())
}

fn qualified_identifier(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split('.').collect();
    ensure!(parts.len() <= 2, "Use a table name or schema.table");
    for part in parts {
        identifier(part)?;
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceBinding {
    pub connection_id: String,
    pub labels: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Join {
    pub name: String,
    pub left_label: String,
    pub left_property: String,
    pub right_label: String,
    pub right_property: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Routine {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub agent: String,
    pub prompt: String,
    pub query: String,
    pub sources: Vec<SourceBinding>,
    pub joins: Vec<Join>,
    pub key_fields: Vec<String>,
    pub operations: Vec<String>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default = "draft")]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

fn draft() -> String {
    "Draft".into()
}

impl Routine {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.id)?;
        ensure!(
            !self.name.trim().is_empty()
                && !self.agent.trim().is_empty()
                && !self.prompt.trim().is_empty(),
            "Name, agent and prompt are required"
        );
        ensure!(
            !self.query.trim().is_empty() && self.query.len() <= 16384,
            "A query of at most 16 KiB is required"
        );
        ensure!(
            !self.sources.is_empty() && !self.key_fields.is_empty(),
            "Select sources and result-key fields"
        );
        ensure!(
            !self.operations.is_empty()
                && self
                    .operations
                    .iter()
                    .all(|op| ["added", "updated", "removed"].contains(&op.as_str())),
            "Select added, updated or removed"
        );
        let mut labels = std::collections::HashSet::new();
        let mut sources = std::collections::HashSet::new();
        for source in &self.sources {
            ensure!(sources.insert(&source.connection_id), "Duplicate source");
            ensure!(
                !source.labels.is_empty(),
                "Assign node labels to each source"
            );
            for label in &source.labels {
                ensure!(
                    labels.insert(label),
                    "A label must belong to exactly one source"
                );
            }
        }
        for join in &self.joins {
            identifier(&join.name)?;
            ensure!(
                labels.contains(&join.left_label) && labels.contains(&join.right_label),
                "Join labels must belong to selected sources"
            );
        }
        Ok(())
    }

    pub fn key(&self, row: &Value) -> Result<Value> {
        let mut key = serde_json::Map::new();
        for field in &self.key_fields {
            let value = row
                .get(field)
                .context("A projected result-key field is missing")?;
            if value.is_null() || value.is_array() || value.is_object() {
                bail!("Result keys must be non-null scalars");
            }
            key.insert(field.clone(), value.clone());
        }
        Ok(Value::Object(key))
    }
}
