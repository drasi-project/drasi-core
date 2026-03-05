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

use anyhow::{anyhow, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::oneshot;

use crate::thread::SqliteParam;
use crate::{RestApiConfig, SqliteSourceHandle, TableKeyConfig};

#[derive(Clone)]
struct RestApiState {
    handle: SqliteSourceHandle,
    allowed_tables: Option<HashSet<String>>,
    table_keys: Vec<TableKeyConfig>,
}

#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub operations: Vec<BatchOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum BatchOperation {
    Insert {
        table: String,
        data: serde_json::Map<String, Value>,
    },
    Update {
        table: String,
        id: String,
        data: serde_json::Map<String, Value>,
    },
    Delete {
        table: String,
        id: String,
    },
}

pub async fn start_rest_api(
    config: RestApiConfig,
    handle: SqliteSourceHandle,
    tables: Option<Vec<String>>,
    table_keys: Vec<TableKeyConfig>,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let allowed_tables = tables.map(|items| items.into_iter().collect::<HashSet<_>>());
    let state = Arc::new(RestApiState {
        handle,
        allowed_tables,
        table_keys,
    });

    let router = Router::new()
        .route("/health", get(health_check))
        .route("/api/tables", get(list_tables))
        .route("/api/tables/:table", get(list_rows).post(insert_row))
        .route(
            "/api/tables/:table/:id",
            get(get_row).put(update_row).delete(delete_row),
        )
        .route("/api/batch", post(batch_operations))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?;

    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    Ok(())
}

async fn health_check() -> Json<Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn list_tables(State(state): State<Arc<RestApiState>>) -> Result<Json<Value>, StatusCode> {
    let rows = state
        .handle
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let names = rows
        .into_iter()
        .filter_map(|row| {
            row.get("name")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .filter(|name| {
            if let Some(allowed) = &state.allowed_tables {
                allowed.contains(name)
            } else {
                true
            }
        })
        .collect::<Vec<_>>();

    Ok(Json(serde_json::json!(names)))
}

async fn list_rows(
    State(state): State<Arc<RestApiState>>,
    Path(table): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST)?;
    let sql = format!("SELECT * FROM {}", quote_ident(&table));
    let rows = state
        .handle
        .query(sql)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!(rows)))
}

async fn get_row(
    State(state): State<Arc<RestApiState>>,
    Path((table, id)): Path<(String, String)>,
) -> Result<Json<Value>, StatusCode> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (where_clause, params) = build_where_by_id(&state, &table, &id)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let sql = format!(
        "SELECT * FROM {} WHERE {}",
        quote_ident(&table),
        where_clause
    );
    let mut rows = state
        .handle
        .query_parameterized(sql, params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Some(row) = rows.pop() {
        Ok(Json(serde_json::json!(row)))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn insert_row(
    State(state): State<Arc<RestApiState>>,
    Path(table): Path<String>,
    Json(data): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, StatusCode> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST)?;
    validate_columns(&data).map_err(|_| StatusCode::BAD_REQUEST)?;

    let columns = data.keys().map(|c| quote_ident(c)).collect::<Vec<_>>();
    let placeholders = (0..data.len()).map(|_| "?").collect::<Vec<_>>();
    let params = data.values().map(json_value_to_param).collect::<Vec<_>>();

    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_ident(&table),
        columns.join(", "),
        placeholders.join(", ")
    );

    state
        .handle
        .execute_parameterized(sql, params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn update_row(
    State(state): State<Arc<RestApiState>>,
    Path((table, id)): Path<(String, String)>,
    Json(data): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, StatusCode> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST)?;
    validate_columns(&data).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (where_clause, where_params) = build_where_by_id(&state, &table, &id)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let set_clause = data
        .keys()
        .map(|col| format!("{} = ?", quote_ident(col)))
        .collect::<Vec<_>>()
        .join(", ");

    let mut params: Vec<SqliteParam> = data.values().map(json_value_to_param).collect();
    params.extend(where_params);

    let sql = format!(
        "UPDATE {} SET {} WHERE {}",
        quote_ident(&table),
        set_clause,
        where_clause
    );

    state
        .handle
        .execute_parameterized(sql, params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn delete_row(
    State(state): State<Arc<RestApiState>>,
    Path((table, id)): Path<(String, String)>,
) -> Result<Json<Value>, StatusCode> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (where_clause, params) = build_where_by_id(&state, &table, &id)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let sql = format!("DELETE FROM {} WHERE {}", quote_ident(&table), where_clause);
    state
        .handle
        .execute_parameterized(sql, params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn batch_operations(
    State(state): State<Arc<RestApiState>>,
    Json(request): Json<BatchRequest>,
) -> Result<Json<Value>, StatusCode> {
    let mut statements = Vec::new();
    for op in &request.operations {
        let (sql, params) = operation_to_parameterized_sql(&state, op)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        statements.push((sql, params));
    }

    state
        .handle
        .execute_parameterized_in_transaction(statements)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn operation_to_parameterized_sql(
    state: &RestApiState,
    operation: &BatchOperation,
) -> Result<(String, Vec<SqliteParam>)> {
    match operation {
        BatchOperation::Insert { table, data } => {
            validate_table_ref(state, table)?;
            validate_columns(data)?;
            let columns = data.keys().map(|c| quote_ident(c)).collect::<Vec<_>>();
            let placeholders = (0..data.len()).map(|_| "?").collect::<Vec<_>>();
            let params = data.values().map(json_value_to_param).collect();
            Ok((
                format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    quote_ident(table),
                    columns.join(", "),
                    placeholders.join(", ")
                ),
                params,
            ))
        }
        BatchOperation::Update { table, id, data } => {
            validate_table_ref(state, table)?;
            validate_columns(data)?;
            let (where_clause, where_params) = build_where_by_id_ref(state, table, id).await?;
            let set_clause = data
                .keys()
                .map(|col| format!("{} = ?", quote_ident(col)))
                .collect::<Vec<_>>()
                .join(", ");
            let mut params: Vec<SqliteParam> = data.values().map(json_value_to_param).collect();
            params.extend(where_params);
            Ok((
                format!(
                    "UPDATE {} SET {} WHERE {}",
                    quote_ident(table),
                    set_clause,
                    where_clause
                ),
                params,
            ))
        }
        BatchOperation::Delete { table, id } => {
            validate_table_ref(state, table)?;
            let (where_clause, params) = build_where_by_id_ref(state, table, id).await?;
            Ok((
                format!("DELETE FROM {} WHERE {}", quote_ident(table), where_clause),
                params,
            ))
        }
    }
}

async fn build_where_by_id(
    state: &RestApiState,
    table: &str,
    id: &str,
) -> Result<(String, Vec<SqliteParam>)> {
    build_where_by_id_ref(state, table, id).await
}

async fn build_where_by_id_ref(
    state: &RestApiState,
    table: &str,
    id: &str,
) -> Result<(String, Vec<SqliteParam>)> {
    let key_columns = if let Some(found) = state.table_keys.iter().find(|k| k.table == table) {
        found.key_columns.clone()
    } else {
        detect_primary_keys(&state.handle, table).await?
    };

    if key_columns.is_empty() {
        return Err(anyhow!("table has no configured or detected primary key"));
    }

    let id_parts = id.split(':').collect::<Vec<_>>();
    if id_parts.len() != key_columns.len() {
        return Err(anyhow!("id parts do not match primary key columns"));
    }

    let where_parts = key_columns
        .iter()
        .map(|column| format!("{} = ?", quote_ident(column)))
        .collect::<Vec<_>>();

    let params = id_parts
        .iter()
        .map(|part| SqliteParam::Text((*part).to_string()))
        .collect();

    Ok((where_parts.join(" AND "), params))
}

async fn detect_primary_keys(handle: &SqliteSourceHandle, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let rows = handle.query(sql).await?;
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

fn validate_table(state: &RestApiState, table: &str) -> Result<()> {
    validate_table_ref(state, table)
}

fn validate_table_ref(state: &RestApiState, table: &str) -> Result<()> {
    if !is_identifier(table) {
        return Err(anyhow!("invalid table name"));
    }

    if let Some(allowed) = &state.allowed_tables {
        if !allowed.contains(table) {
            return Err(anyhow!("table not allowed"));
        }
    }

    Ok(())
}

fn validate_columns(data: &serde_json::Map<String, Value>) -> Result<()> {
    for key in data.keys() {
        if !is_identifier(key) {
            return Err(anyhow!("invalid column name"));
        }
    }
    Ok(())
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn json_value_to_param(value: &Value) -> SqliteParam {
    match value {
        Value::Null => SqliteParam::Null,
        Value::Bool(v) => SqliteParam::Bool(*v),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                SqliteParam::Integer(i)
            } else if let Some(f) = v.as_f64() {
                SqliteParam::Real(f)
            } else {
                SqliteParam::Text(v.to_string())
            }
        }
        Value::String(v) => SqliteParam::Text(v.clone()),
        Value::Array(_) | Value::Object(_) => SqliteParam::Text(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_validation_accepts_and_rejects_expected_values() {
        assert!(is_identifier("sensors"));
        assert!(is_identifier("_events"));
        assert!(!is_identifier("9table"));
        assert!(!is_identifier("table-name"));
        assert!(!is_identifier("table name"));
    }

    #[test]
    fn quote_ident_escapes_double_quotes() {
        assert_eq!(quote_ident("simple"), "\"simple\"");
        assert_eq!(quote_ident("my\"table"), "\"my\"\"table\"");
    }

    #[test]
    fn json_value_to_param_handles_core_types() {
        assert!(matches!(
            json_value_to_param(&Value::Null),
            SqliteParam::Null
        ));
        assert!(matches!(
            json_value_to_param(&Value::Bool(true)),
            SqliteParam::Bool(true)
        ));
        assert!(matches!(
            json_value_to_param(&Value::Bool(false)),
            SqliteParam::Bool(false)
        ));
        assert!(matches!(
            json_value_to_param(&Value::Number(42.into())),
            SqliteParam::Integer(42)
        ));
        match json_value_to_param(&Value::String("hello".to_string())) {
            SqliteParam::Text(s) => assert_eq!(s, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
        match json_value_to_param(&serde_json::json!({"x": "y"})) {
            SqliteParam::Text(s) => assert!(s.contains("\"x\"")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn json_value_to_param_handles_strings_with_quotes() {
        match json_value_to_param(&Value::String("O'Reilly".to_string())) {
            SqliteParam::Text(s) => assert_eq!(s, "O'Reilly"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn validate_columns_rejects_invalid_column_names() {
        let mut valid = serde_json::Map::new();
        valid.insert("col_1".to_string(), Value::String("ok".to_string()));
        assert!(validate_columns(&valid).is_ok());

        let mut invalid = serde_json::Map::new();
        invalid.insert("bad-column".to_string(), Value::String("oops".to_string()));
        assert!(validate_columns(&invalid).is_err());
    }
}
