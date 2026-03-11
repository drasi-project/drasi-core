// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Primary key discovery and Oracle element ID generation.

use crate::config::{validate_sql_identifier, OracleSourceConfig};
use crate::error::OracleError;
use crate::types::value_to_string;
use anyhow::Result;
use drasi_core::models::{ElementPropertyMap, ElementValue};
use oracle::Connection;
use std::collections::HashMap;

pub struct PrimaryKeyCache {
    keys: HashMap<String, Vec<String>>,
}

impl PrimaryKeyCache {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
        }
    }

    pub fn discover_keys(&mut self, conn: &Connection, config: &OracleSourceConfig) -> Result<()> {
        let mut table_map: HashMap<String, Vec<String>> = HashMap::new();

        for table in &config.tables {
            validate_sql_identifier(table)?;
            let (owner, table_name) = split_table_name(table, &config.user)?;
            table_map.entry(owner).or_default().push(table_name);
        }

        for (owner, tables) in table_map {
            if tables.is_empty() {
                continue;
            }

            let query = format!(
                "SELECT ac.owner, acc.table_name, acc.column_name, acc.position
                 FROM all_constraints ac
                 JOIN all_cons_columns acc
                   ON ac.owner = acc.owner
                  AND ac.constraint_name = acc.constraint_name
                 WHERE ac.constraint_type = 'P'
                   AND ac.owner = '{}'
                   AND acc.table_name IN ({})
                 ORDER BY ac.owner, acc.table_name, acc.position",
                owner,
                tables
                    .iter()
                    .map(|table| format!("'{table}'"))
                    .collect::<Vec<_>>()
                    .join(",")
            );

            let rows = conn.query(&query, &[])?;
            for row in rows {
                let row = row?;
                let owner: String = row.get(0)?;
                let table_name: String = row.get(1)?;
                let column_name: String = row.get(2)?;
                self.keys
                    .entry(format!(
                        "{}.{}",
                        owner.to_uppercase(),
                        table_name.to_uppercase()
                    ))
                    .or_default()
                    .push(column_name.to_uppercase());
            }
        }

        for override_key in &config.table_keys {
            let (owner, table_name) = split_table_name(&override_key.table, &config.user)?;
            self.keys.insert(
                format!("{owner}.{table_name}"),
                override_key
                    .key_columns
                    .iter()
                    .map(|column| column.to_uppercase())
                    .collect(),
            );
        }

        Ok(())
    }

    pub fn get(&self, schema: &str, table: &str) -> Option<&Vec<String>> {
        self.keys
            .get(&format!(
                "{}.{}",
                schema.to_uppercase(),
                table.to_uppercase()
            ))
            .or_else(|| self.keys.get(&table.to_uppercase()))
    }

    pub fn make_element_id(
        &self,
        schema: &str,
        table: &str,
        properties: &ElementPropertyMap,
    ) -> Result<String> {
        let pk_columns = self.get(schema, table).ok_or_else(|| {
            OracleError::PrimaryKey(format!(
                "No primary key configured for table '{schema}.{table}'"
            ))
        })?;

        let mut parts = Vec::with_capacity(pk_columns.len());
        for column in pk_columns {
            let lookup = column.to_lowercase();
            let value = properties.get(&lookup).ok_or_else(|| {
                OracleError::PrimaryKey(format!(
                    "Primary key column '{column}' not found in row for '{schema}.{table}'"
                ))
            })?;

            if matches!(value, ElementValue::Null) {
                return Err(OracleError::PrimaryKey(format!(
                    "Primary key column '{column}' is NULL for '{schema}.{table}'"
                ))
                .into());
            }

            parts.push(value_to_string(value));
        }

        Ok(format!(
            "{}:{}:{}",
            schema.to_lowercase(),
            table.to_lowercase(),
            parts.join(":")
        ))
    }

    pub fn make_element_id_from_values(
        &self,
        schema: &str,
        table: &str,
        values: &HashMap<String, String>,
    ) -> Result<String> {
        let pk_columns = self.get(schema, table).ok_or_else(|| {
            OracleError::PrimaryKey(format!(
                "No primary key configured for table '{schema}.{table}'"
            ))
        })?;

        let mut parts = Vec::with_capacity(pk_columns.len());
        for column in pk_columns {
            let lookup = column.to_lowercase();
            let value = values.get(&lookup).ok_or_else(|| {
                OracleError::PrimaryKey(format!(
                    "Primary key column '{column}' not found in SQL_UNDO for '{schema}.{table}'"
                ))
            })?;
            parts.push(normalize_sql_literal(value));
        }

        Ok(format!(
            "{}:{}:{}",
            schema.to_lowercase(),
            table.to_lowercase(),
            parts.join(":")
        ))
    }
}

impl Default for PrimaryKeyCache {
    fn default() -> Self {
        Self::new()
    }
}

pub fn split_table_name(table: &str, default_owner: &str) -> Result<(String, String)> {
    validate_sql_identifier(table)?;
    let mut parts = table.split('.');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if let Some(table_name) = second {
        Ok((first.to_uppercase(), table_name.to_uppercase()))
    } else {
        Ok((default_owner.to_uppercase(), first.to_uppercase()))
    }
}

fn normalize_sql_literal(literal: &str) -> String {
    let trimmed = literal.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return "null".to_string();
    }

    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        return trimmed[1..trimmed.len() - 1].replace("''", "'");
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::ElementPropertyMap;

    #[test]
    fn test_make_element_id() {
        let mut cache = PrimaryKeyCache::new();
        cache
            .keys
            .insert("HR.EMPLOYEES".to_string(), vec!["EMPLOYEE_ID".to_string()]);

        let mut props = ElementPropertyMap::new();
        props.insert("employee_id", ElementValue::Integer(42));
        let element_id = cache.make_element_id("HR", "EMPLOYEES", &props).unwrap();
        assert_eq!(element_id, "hr:employees:42");
    }

    #[test]
    fn test_make_element_id_from_values() {
        let mut cache = PrimaryKeyCache::new();
        cache
            .keys
            .insert("HR.EMPLOYEES".to_string(), vec!["EMPLOYEE_ID".to_string()]);

        let values = HashMap::from([(String::from("employee_id"), String::from("'42'"))]);
        let element_id = cache
            .make_element_id_from_values("HR", "EMPLOYEES", &values)
            .unwrap();
        assert_eq!(element_id, "hr:employees:42");
    }

    #[test]
    fn test_split_table_name_uses_default_owner() {
        let (schema, table) = split_table_name("employees", "hr").unwrap();
        assert_eq!(schema, "HR");
        assert_eq!(table, "EMPLOYEES");
    }
}
