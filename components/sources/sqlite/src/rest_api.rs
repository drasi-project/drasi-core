// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{thread::SqliteParam, SqliteSourceHandle};
use anyhow::Result;
use drasi_sqlite_common::rest_api::SqliteRestClient;
use serde_json::Value;

pub(crate) use drasi_sqlite_common::rest_api::run_rest_api;

#[async_trait::async_trait]
impl SqliteRestClient for SqliteSourceHandle {
    async fn query(
        &self,
        sql: String,
        params: Vec<SqliteParam>,
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        self.query_parameterized(sql, params).await
    }
    async fn execute(&self, sql: String, params: Vec<SqliteParam>) -> Result<usize> {
        self.execute_parameterized(sql, params).await
    }
    async fn execute_transaction(&self, statements: Vec<(String, Vec<SqliteParam>)>) -> Result<()> {
        self.execute_parameterized_in_transaction(statements).await
    }
    async fn key_columns(&self, table: &str) -> Result<Vec<String>> {
        let sql = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
        let rows = self.query(sql).await?;
        let mut keys = rows
            .into_iter()
            .filter_map(|row| {
                let name = row.get("name").and_then(Value::as_str)?;
                let pk = row.get("pk").and_then(Value::as_i64).unwrap_or_default();
                if pk > 0 {
                    Some((pk, name.to_string()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        keys.sort_by_key(|(order, _)| *order);
        Ok(keys.into_iter().map(|(_, name)| name).collect())
    }
}
