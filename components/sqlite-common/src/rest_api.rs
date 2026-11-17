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

//! The SQLite REST routes, shared by the legacy source and the native source.
//!
//! [`run_rest_api`] keeps the legacy behaviour: no authentication, every row
//! returned by `GET /api/tables/{table}`. [`router`] builds the same routes
//! with [`RestOptions`]: bearer authentication, a body-size limit, a request
//! deadline, a pause gate (503 with `Retry-After`), paging and the optional
//! authenticated `/api/sql` route. The caller owns the listener and its
//! connection limits.

use anyhow::{anyhow, Result};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use crate::{SqliteParam, TableKeyConfig};

#[async_trait::async_trait]
pub trait SqliteRestClient: Send + Sync {
    async fn query(
        &self,
        sql: String,
        params: Vec<SqliteParam>,
    ) -> Result<Vec<serde_json::Map<String, Value>>>;
    async fn execute(&self, sql: String, params: Vec<SqliteParam>) -> Result<usize>;
    async fn execute_transaction(&self, statements: Vec<(String, Vec<SqliteParam>)>) -> Result<()>;
    async fn key_columns(&self, table: &str) -> Result<Vec<String>>;
    /// Runs `statements` in one transaction and returns each affected-row
    /// count. Only clients that support the `/api/sql` route implement it.
    async fn execute_counted(
        &self,
        _statements: Vec<(String, Vec<SqliteParam>)>,
    ) -> Result<Vec<usize>> {
        Err(anyhow!("this SQLite client does not support /api/sql"))
    }
}

#[cfg(feature = "native-runtime")]
#[async_trait::async_trait]
impl SqliteRestClient for crate::native::SqliteHandle {
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
        self.execute_counted(statements).await.map(|_| ())
    }
    async fn key_columns(&self, table: &str) -> Result<Vec<String>> {
        crate::native::SqliteHandle::key_columns(self, table).await
    }
    async fn execute_counted(
        &self,
        statements: Vec<(String, Vec<SqliteParam>)>,
    ) -> Result<Vec<usize>> {
        self.transaction(|transaction| async move {
            let mut counts = Vec::with_capacity(statements.len());
            for (sql, params) in statements {
                counts.push(transaction.execute_parameterized(sql, params).await?);
            }
            Ok(counts)
        })
        .await
    }
}

/// Checks an `Authorization` header value; `true` admits the request.
pub type Authorize = Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>;

/// Hardening and feature options for [`router`].
#[derive(Clone)]
pub struct RestOptions {
    /// Tables the routes may touch; `None` allows every application table.
    pub tables: Option<Vec<String>>,
    pub table_keys: Vec<TableKeyConfig>,
    /// Bearer authentication for every route except `/health`; `None`
    /// admits every request.
    pub authorize: Option<Authorize>,
    /// Largest accepted request body, in bytes (413 above it).
    pub max_body_bytes: usize,
    /// Deadline for one request (504 when it expires). A write whose commit
    /// was already submitted may still complete.
    pub request_timeout: Option<Duration>,
    /// `true` refuses writes with 503 and `Retry-After`.
    pub paused: Option<watch::Receiver<bool>>,
    /// `true` refuses every request with 503.
    pub stopping: watch::Receiver<bool>,
    /// Largest page `GET /api/tables/{table}` returns; `None` returns every
    /// row in one response (the legacy behaviour).
    pub page_limit: Option<usize>,
    /// Enables the authenticated `POST /api/sql` route.
    pub sql_api: bool,
    /// Seconds advertised in `Retry-After` while paused.
    pub retry_after_secs: u64,
}

impl RestOptions {
    /// The legacy route set: unauthenticated, unpaged, no SQL route.
    pub fn legacy(
        tables: Option<Vec<String>>,
        table_keys: Vec<TableKeyConfig>,
        stopping: watch::Receiver<bool>,
    ) -> Self {
        Self {
            tables,
            table_keys,
            authorize: None,
            max_body_bytes: 2 * 1024 * 1024,
            request_timeout: None,
            paused: None,
            stopping,
            page_limit: None,
            sql_api: false,
            retry_after_secs: 1,
        }
    }
}

#[derive(Clone)]
struct RestApiState {
    handle: Arc<dyn SqliteRestClient>,
    allowed_tables: Option<HashSet<String>>,
    table_keys: Vec<TableKeyConfig>,
    page_limit: Option<usize>,
    retry_after_secs: u64,
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

/// One parameterized statement of an `/api/sql` request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlStatement {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

/// `POST /api/sql`: either `{"execute": [statements]}`, run in one
/// transaction, or `{"query": statement}` for one read-only statement.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum SqlRequest {
    Execute(Vec<SqlStatement>),
    Query(SqlStatement),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    limit: Option<usize>,
    offset: Option<usize>,
}

/// The legacy entry point: unauthenticated routes served with axum until
/// `shutdown_rx` becomes true.
pub async fn run_rest_api(
    listener: tokio::net::TcpListener,
    handle: impl SqliteRestClient + 'static,
    tables: Option<Vec<String>>,
    table_keys: Vec<TableKeyConfig>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let router = router(
        handle,
        RestOptions::legacy(tables, table_keys, shutdown_rx.clone()),
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.wait_for(|stopped| *stopped).await;
        })
        .await
}

/// The SQLite routes with `options` applied.
pub fn router(handle: impl SqliteRestClient + 'static, options: RestOptions) -> Router {
    let allowed_tables = options
        .tables
        .clone()
        .map(|items| items.into_iter().collect::<HashSet<_>>());
    let state = Arc::new(RestApiState {
        handle: Arc::new(handle),
        allowed_tables,
        table_keys: options.table_keys.clone(),
        page_limit: options.page_limit,
        retry_after_secs: options.retry_after_secs,
    });
    let mut api = Router::new()
        .route("/api/tables", get(list_tables))
        .route("/api/tables/:table", get(list_rows).post(insert_row))
        .route(
            "/api/tables/:table/:id",
            get(get_row).put(update_row).delete(delete_row),
        )
        .route("/api/batch", post(batch_operations));
    if options.sql_api {
        api = api.route("/api/sql", post(sql_route));
    }
    let gate = Arc::new(Gate {
        authorize: options.authorize.clone(),
        paused: options.paused.clone(),
        timeout: options.request_timeout,
        retry_after_secs: options.retry_after_secs,
    });
    let api = api
        .with_state(state)
        .layer(DefaultBodyLimit::max(options.max_body_bytes))
        .layer(middleware::from_fn_with_state(gate, guard));
    Router::new()
        .route("/health", get(health_check))
        .merge(api)
        .layer(middleware::from_fn_with_state(
            options.stopping.clone(),
            reject_after_shutdown,
        ))
}

struct Gate {
    authorize: Option<Authorize>,
    paused: Option<watch::Receiver<bool>>,
    timeout: Option<Duration>,
    retry_after_secs: u64,
}

fn retry_later(seconds: u64, message: &'static str) -> Response {
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, message).into_response();
    if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// Authentication, then the pause gate for writes, then the request deadline.
async fn guard(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    if let Some(authorize) = &gate.authorize {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if !authorize(presented) {
            log::debug!("SQLite REST request refused: missing or invalid bearer token");
            let mut response = StatusCode::UNAUTHORIZED.into_response();
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return response;
        }
    }
    let writes = request.method() != Method::GET || request.uri().path() == "/api/sql";
    if writes && gate.paused.as_ref().is_some_and(|paused| *paused.borrow()) {
        return retry_later(gate.retry_after_secs, "SQLite source is paused");
    }
    match gate.timeout {
        Some(timeout) => match tokio::time::timeout(timeout, next.run(request)).await {
            Ok(response) => response,
            Err(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                "SQLite request deadline exceeded",
            )
                .into_response(),
        },
        None => next.run(request).await,
    }
}

async fn reject_after_shutdown(
    State(shutdown): State<watch::Receiver<bool>>,
    request: Request,
    next: Next,
) -> Response {
    let stopped = *shutdown.borrow();
    if stopped {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    next.run(request).await
}

/// Maps a worker failure: paused or stopping become 503, anything else 500.
fn failure(state: &RestApiState, error: &anyhow::Error) -> Response {
    #[cfg(feature = "native-runtime")]
    {
        if crate::native::error::is_paused(error) {
            return retry_later(state.retry_after_secs, "SQLite source is paused");
        }
        if crate::native::error::is_stopping(error) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let _ = state;
    log::debug!("SQLite REST request failed: {error:#}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

async fn health_check() -> Json<Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn list_tables(State(state): State<Arc<RestApiState>>) -> Result<Json<Value>, Response> {
    let rows = state
        .handle
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND lower(substr(name,1,7))!='sqlite_' AND lower(substr(name,1,8))!='__drasi_' ORDER BY name".into(),
            Vec::new(),
        )
        .await
        .map_err(|error| failure(&state, &error))?;

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

/// Without a page limit, every row in one array (legacy). With one, at most
/// `limit` rows from `offset`, ordered by the table key; `X-Next-Offset`
/// names the next page when more rows exist.
async fn list_rows(
    State(state): State<Arc<RestApiState>>,
    Path(table): Path<String>,
    Query(page): Query<Page>,
) -> Result<Response, Response> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let Some(maximum) = state.page_limit else {
        let sql = format!("SELECT * FROM {}", quote_ident(&table));
        let rows = state
            .handle
            .query(sql, Vec::new())
            .await
            .map_err(|error| failure(&state, &error))?;
        return Ok(Json(serde_json::json!(rows)).into_response());
    };
    let limit = page.limit.unwrap_or(maximum).clamp(1, maximum);
    let offset = page.offset.unwrap_or(0);
    let keys = key_columns(&state, &table)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let order = keys
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT * FROM {} ORDER BY {order} LIMIT ? OFFSET ?",
        quote_ident(&table)
    );
    let bad = |_| StatusCode::BAD_REQUEST.into_response();
    let fetch = i64::try_from(limit).map_err(bad)?;
    let skip = i64::try_from(offset).map_err(bad)?;
    let next = offset
        .checked_add(limit)
        .and_then(|next| i64::try_from(next).ok())
        .ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;
    let rows = state
        .handle
        .query(
            sql,
            vec![SqliteParam::Integer(fetch), SqliteParam::Integer(skip)],
        )
        .await
        .map_err(|error| failure(&state, &error))?;
    // A page never exceeds the worker's read bound, so the next-page probe is
    // a separate one-row read.
    let more = rows.len() == limit
        && !state
            .handle
            .query(
                format!(
                    "SELECT 1 AS more FROM {} ORDER BY {order} LIMIT 1 OFFSET ?",
                    quote_ident(&table)
                ),
                vec![SqliteParam::Integer(next)],
            )
            .await
            .map_err(|error| failure(&state, &error))?
            .is_empty();
    let mut response = Json(serde_json::json!(rows)).into_response();
    if more {
        if let Ok(value) = HeaderValue::from_str(&(offset + limit).to_string()) {
            response.headers_mut().insert("x-next-offset", value);
        }
    }
    Ok(response)
}

async fn get_row(
    State(state): State<Arc<RestApiState>>,
    Path((table, id)): Path<(String, String)>,
) -> Result<Json<Value>, Response> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let (where_clause, params) = build_where_by_id(&state, &table, &id)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let sql = format!(
        "SELECT * FROM {} WHERE {}",
        quote_ident(&table),
        where_clause
    );
    let mut rows = state
        .handle
        .query(sql, params)
        .await
        .map_err(|error| failure(&state, &error))?;

    if let Some(row) = rows.pop() {
        Ok(Json(serde_json::json!(row)))
    } else {
        Err(StatusCode::NOT_FOUND.into_response())
    }
}

async fn insert_row(
    State(state): State<Arc<RestApiState>>,
    Path(table): Path<String>,
    Json(data): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, Response> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    validate_columns(&data).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;

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
        .execute(sql, params)
        .await
        .map_err(|error| failure(&state, &error))?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn update_row(
    State(state): State<Arc<RestApiState>>,
    Path((table, id)): Path<(String, String)>,
    Json(data): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, Response> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    validate_columns(&data).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let (where_clause, where_params) = build_where_by_id(&state, &table, &id)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;

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
        .execute(sql, params)
        .await
        .map_err(|error| failure(&state, &error))?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn delete_row(
    State(state): State<Arc<RestApiState>>,
    Path((table, id)): Path<(String, String)>,
) -> Result<Json<Value>, Response> {
    validate_table(&state, &table).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let (where_clause, params) = build_where_by_id(&state, &table, &id)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;

    let sql = format!("DELETE FROM {} WHERE {}", quote_ident(&table), where_clause);
    state
        .handle
        .execute(sql, params)
        .await
        .map_err(|error| failure(&state, &error))?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn batch_operations(
    State(state): State<Arc<RestApiState>>,
    Json(request): Json<BatchRequest>,
) -> Result<Json<Value>, Response> {
    let mut statements = Vec::new();
    for op in &request.operations {
        let (sql, params) = operation_to_parameterized_sql(&state, op)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
        statements.push((sql, params));
    }

    state
        .handle
        .execute_transaction(statements)
        .await
        .map_err(|error| failure(&state, &error))?;

    Ok(Json(serde_json::json!({ "success": true })))
}

/// `POST /api/sql`. Statements run through the source's own connection, under
/// its authorizer: DDL that drops or alters tables, pragmas, transaction
/// control, attachments and journal objects are refused. SQL errors return
/// 400 with the message, since the caller is authenticated.
async fn sql_route(
    State(state): State<Arc<RestApiState>>,
    Json(request): Json<SqlRequest>,
) -> Result<Json<Value>, Response> {
    let bad_request = |error: &anyhow::Error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{error:#}") })),
        )
            .into_response()
    };
    let refused = |error: anyhow::Error| {
        let response = failure(&state, &error);
        if response.status() == StatusCode::INTERNAL_SERVER_ERROR {
            bad_request(&error)
        } else {
            response
        }
    };
    match request {
        SqlRequest::Query(statement) => {
            let params = statement.params.iter().map(json_value_to_param).collect();
            let rows = state
                .handle
                .query(statement.sql, params)
                .await
                .map_err(refused)?;
            Ok(Json(serde_json::json!({ "rows": rows })))
        }
        SqlRequest::Execute(statements) => {
            if statements.is_empty() {
                return Err(bad_request(&anyhow!(
                    "execute requires at least one statement"
                )));
            }
            let statements = statements
                .into_iter()
                .map(|statement| {
                    let params = statement.params.iter().map(json_value_to_param).collect();
                    (statement.sql, params)
                })
                .collect();
            let counts = state
                .handle
                .execute_counted(statements)
                .await
                .map_err(refused)?;
            Ok(Json(serde_json::json!({ "rowsAffected": counts })))
        }
    }
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

async fn key_columns(state: &RestApiState, table: &str) -> Result<Vec<String>> {
    let keys = if let Some(found) = state.table_keys.iter().find(|k| k.table == table) {
        found.key_columns.clone()
    } else {
        state.handle.key_columns(table).await?
    };
    if keys.is_empty() {
        return Err(anyhow!("table has no configured or detected primary key"));
    }
    Ok(keys)
}

async fn build_where_by_id_ref(
    state: &RestApiState,
    table: &str,
    id: &str,
) -> Result<(String, Vec<SqliteParam>)> {
    let key_columns = key_columns(state, table).await?;

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

fn validate_table(state: &RestApiState, table: &str) -> Result<()> {
    validate_table_ref(state, table)
}

fn validate_table_ref(state: &RestApiState, table: &str) -> Result<()> {
    if !is_identifier(table) || table.to_ascii_lowercase().starts_with("__drasi_") {
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
