// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::{Connection, Routine},
    foundry::Foundry,
    runtime::Runtime,
};
use anyhow::{ensure, Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, convert::Infallible, sync::Arc};
use tokio::sync::Mutex;

pub struct App {
    pub runtime: Arc<Runtime>,
    pub foundry: Foundry,
    pub fixtures: bool,
    pub port: u16,
    pub shutdown: tokio::sync::watch::Sender<bool>,
    previews: Mutex<HashMap<String, String>>,
    operations: Mutex<()>,
}

pub struct ApiError(anyhow::Error, StatusCode);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self(error.into(), StatusCode::BAD_REQUEST)
    }
}
impl ApiError {
    fn conflict(message: &str) -> Self {
        Self(anyhow::anyhow!("{message}"), StatusCode::CONFLICT)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let correlation_id = uuid::Uuid::new_v4().to_string();
        log::error!("API {correlation_id}: {:#}", self.0);
        (self.1,Json(json!({"code":"operation_failed","message":self.0.to_string(),"correlationId":correlation_id}))).into_response()
    }
}
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;
fn require(condition: bool, message: &str) -> Result<()> {
    ensure!(condition, "{message}");
    Ok(())
}

impl App {
    pub fn new(runtime: Arc<Runtime>, foundry: Foundry, fixtures: bool, port: u16) -> Arc<Self> {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            runtime,
            foundry,
            fixtures,
            port,
            shutdown,
            previews: Mutex::new(HashMap::new()),
            operations: Mutex::new(()),
        })
    }
    pub fn router(app: Arc<Self>) -> Router {
        Router::new()
            .route("/api/health", get(health))
            .route("/api/shutdown", post(shutdown))
            .route("/api/foundry/status", get(foundry_status))
            .route("/api/agents", get(agents))
            .route("/api/connections", get(connections).post(save_connection))
            .route("/api/connections/validate", post(validate_connection))
            .route("/api/connections/{id}/schema", get(schema))
            .route("/api/routines", get(routines).post(save_routine))
            .route(
                "/api/routines/{id}",
                get(routine).put(edit_routine).delete(remove_routine),
            )
            .route("/api/routines/{id}/{action}", post(action))
            .route("/api/routines/{id}/results", get(results))
            .route("/api/routines/{id}/runs", get(runs))
            .route("/api/runs/{id}", get(run))
            .route("/api/query/validate", post(validate_query))
            .route("/api/previews", post(preview))
            .route(
                "/api/previews/{id}",
                get(preview_results).delete(close_preview),
            )
            .route("/api/demo/actions", get(demo_actions))
            .route("/api/demo/actions/{id}", post(demo_action))
            .route("/api/events", get(events))
            .fallback_service(
                tower_http::services::ServeDir::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/ui/dist"
                ))
                .append_index_html_on_directories(true),
            )
            .layer(axum::extract::DefaultBodyLimit::max(65536))
            .layer(middleware::from_fn_with_state(app.clone(), origin_guard))
            .with_state(app)
    }
}

async fn origin_guard(
    State(app): State<Arc<App>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let allowed_host = [
        format!("127.0.0.1:{}", app.port),
        format!("localhost:{}", app.port),
    ];
    let host = request.headers().get("host").and_then(|v| v.to_str().ok());
    if host.is_some_and(|h| !allowed_host.iter().any(|a| a == h)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(origin) = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
    {
        let allowed = [
            format!("http://127.0.0.1:{}", app.port),
            format!("http://localhost:{}", app.port),
        ];
        if !allowed.iter().any(|o| o == origin) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(request).await
}

async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    Json(
        json!({"ready":true,"mode":if app.foundry.offline {"offline"} else {"live"},"fixtures":app.fixtures}),
    )
}
async fn shutdown(State(app): State<Arc<App>>) -> Json<Value> {
    app.shutdown.send_replace(true);
    Json(json!({"ok":true}))
}
async fn foundry_status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.foundry.status())
}
async fn agents(State(app): State<Arc<App>>) -> ApiResult<Value> {
    Ok(Json(app.foundry.agents().await?))
}
async fn connections(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(app
        .runtime
        .store
        .read()
        .await
        .connections
        .values()
        .collect::<Vec<_>>()))
}
async fn routines(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(app
        .runtime
        .store
        .read()
        .await
        .routines
        .values()
        .collect::<Vec<_>>()))
}
async fn routine(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Routine> {
    Ok(Json(
        app.runtime
            .store
            .read()
            .await
            .routines
            .get(&id)
            .context("Routine not found")?
            .clone(),
    ))
}

async fn save_connection(
    State(app): State<Arc<App>>,
    Json(mut connection): Json<Connection>,
) -> ApiResult<Connection> {
    let _operation = app.operations.lock().await;
    if connection.id.is_empty() {
        connection.id = uuid::Uuid::new_v4().to_string();
    }
    connection.validate()?;
    app.runtime
        .store
        .update(|records| {
            ensure!(
                !records.routines.values().any(|r| r
                    .sources
                    .iter()
                    .any(|s| s.connection_id == connection.id)
                    && ["Active", "Starting"].contains(&r.status.as_str())),
                "Pause dependent routines before editing a source"
            );
            records
                .connections
                .insert(connection.id.clone(), connection.clone());
            Ok(())
        })
        .await?;
    // A changed source must not silently reuse an old runtime configuration.
    let dependents: Vec<_> = app
        .runtime
        .store
        .read()
        .await
        .routines
        .values()
        .filter(|r| r.sources.iter().any(|s| s.connection_id == connection.id))
        .map(|r| r.id.clone())
        .collect();
    for id in dependents {
        app.runtime.remove(&id).await?;
    }
    if app
        .runtime
        .core
        .list_sources()
        .await?
        .iter()
        .any(|(id, _)| id == &connection.id)
    {
        app.runtime
            .core
            .remove_source(&connection.id, false)
            .await?;
    }
    Ok(Json(connection))
}
async fn validate_connection(Json(connection): Json<Connection>) -> ApiResult<Value> {
    Ok(Json(crate::metadata::validate(&connection).await?))
}
async fn schema(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Value> {
    let connection = app
        .runtime
        .store
        .read()
        .await
        .connections
        .get(&id)
        .context("Connection not found")?
        .clone();
    Ok(Json(crate::metadata::validate(&connection).await?))
}

fn validate_query_definition(routine: &Routine) -> Result<()> {
    ensure!(
        !routine.sources.is_empty() && !routine.query.trim().is_empty(),
        "Select sources and enter a query"
    );
    let query = routine.query.replace("\r\n", "\n").replace('\r', "\n");
    let labels = drasi_lib::queries::LabelExtractor::extract_labels(
        &query,
        &drasi_lib::config::QueryLanguage::Cypher,
    )?;
    ensure!(
        labels
            .node_labels
            .iter()
            .all(|label| routine.sources.iter().any(|s| s.labels.contains(label))),
        "Every query label must be mapped to a source"
    );
    for join in &routine.joins {
        ensure!(
            labels.node_labels.contains(&join.left_label)
                && labels.node_labels.contains(&join.right_label),
            "Join labels must appear in the query"
        );
    }
    Ok(())
}
async fn validate_query(Json(routine): Json<Routine>) -> ApiResult<Value> {
    validate_query_definition(&routine)?;
    Ok(Json(json!({"valid":true})))
}

async fn save_routine(
    State(app): State<Arc<App>>,
    Json(mut routine): Json<Routine>,
) -> ApiResult<Routine> {
    let _operation = app.operations.lock().await;
    if routine.id.is_empty() {
        routine.id = uuid::Uuid::new_v4().to_string();
    }
    routine.validate()?;
    validate_query_definition(&routine)?;
    routine.revision = 1;
    routine.status = "Draft".into();
    routine.error = None;
    app.runtime
        .store
        .update(|records| {
            ensure!(
                !records.routines.contains_key(&routine.id),
                "Routine ID already exists"
            );
            records.routines.insert(routine.id.clone(), routine.clone());
            Ok(())
        })
        .await?;
    Ok(Json(routine))
}
async fn edit_routine(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(mut routine): Json<Routine>,
) -> ApiResult<Routine> {
    let _operation = app.operations.lock().await;
    require(routine.id == id, "Routine ID cannot change")?;
    routine.validate()?;
    validate_query_definition(&routine)?;
    let original = app
        .runtime
        .store
        .read()
        .await
        .routines
        .get(&id)
        .context("Routine not found")?
        .clone();
    require(
        original.status != "Active" && original.status != "Starting",
        "Pause before editing",
    )?;
    if original.revision != routine.revision {
        return Err(ApiError::conflict(
            "Revision conflict; reload before saving",
        ));
    }
    app.runtime.remove(&id).await?;
    routine.revision += 1;
    routine.status = "Draft".into();
    routine.error = None;
    app.runtime
        .store
        .update(|r| {
            ensure!(
                r.routines
                    .get(&id)
                    .is_some_and(|old| old.revision + 1 == routine.revision),
                "Revision conflict"
            );
            r.routines.insert(id, routine.clone());
            Ok(())
        })
        .await?;
    Ok(Json(routine))
}
async fn remove_routine(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Value> {
    let _operation = app.operations.lock().await;
    app.runtime.remove(&id).await?;
    app.runtime
        .store
        .update(|r| {
            ensure!(r.routines.remove(&id).is_some(), "Routine not found");
            for run in r
                .runs
                .values_mut()
                .filter(|run| run.routine_id == id && run.status == "Queued")
            {
                run.status = "Cancelled".into();
            }
            Ok(())
        })
        .await?;
    Ok(Json(json!({"ok":true})))
}

async fn action(
    State(app): State<Arc<App>>,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult<Routine> {
    let _operation = app.operations.lock().await;
    let saved = app
        .runtime
        .store
        .read()
        .await
        .routines
        .get(&id)
        .context("Routine not found")?
        .clone();
    match action.as_str() {
        "activate" | "resume" => {
            require(
                saved.status != "Active" && saved.status != "Starting",
                "Routine already active",
            )?;
            app.foundry.ready().await?;
            if saved.status == "Error" {
                app.runtime.remove(&id).await?;
                app.runtime
                    .store
                    .update(|records| {
                        for run in records
                            .runs
                            .values_mut()
                            .filter(|run| run.routine_id == id && run.status == "Queued")
                        {
                            run.status = "Cancelled".into();
                        }
                        Ok(())
                    })
                    .await?;
            }
            app.runtime
                .store
                .update(|r| {
                    let routine = r.routines.get_mut(&id).context("Routine not found")?;
                    routine.status = "Starting".into();
                    routine.error = None;
                    Ok(())
                })
                .await?;
            let result = async {
                if !app.runtime.live.lock().await.contains_key(&id) {
                    app.runtime
                        .start(
                            &saved,
                            if action == "activate" {
                                crate::runtime::StartMode::Active
                            } else {
                                crate::runtime::StartMode::Paused
                            },
                        )
                        .await?;
                }
                if action == "resume" {
                    let handle = app
                        .runtime
                        .live
                        .lock()
                        .await
                        .get(&id)
                        .context("Runtime missing")?
                        .handle
                        .clone();
                    let mut gate = handle.gate.lock().await;
                    let snapshot = handle.snapshot().await?;
                    gate.cutoff = snapshot["sequence"]
                        .as_str()
                        .context("Missing cutoff")?
                        .parse()?;
                    gate.enabled = true;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                app.runtime
                    .store
                    .update(|r| {
                        let routine = r.routines.get_mut(&id).context("Routine not found")?;
                        routine.status = "Error".into();
                        routine.error = Some(error.to_string());
                        Ok(())
                    })
                    .await?;
                return Err(error.into());
            }
            app.runtime
                .store
                .update(|r| {
                    let routine = r.routines.get_mut(&id).context("Routine not found")?;
                    routine.status = "Active".into();
                    Ok(())
                })
                .await?;
        }
        "pause" => {
            let handle = app
                .runtime
                .live
                .lock()
                .await
                .get(&id)
                .context("Routine is not running")?
                .handle
                .clone();
            let mut gate = handle.gate.lock().await;
            gate.enabled = false;
            app.runtime
                .store
                .update(|r| {
                    let routine = r.routines.get_mut(&id).context("Routine not found")?;
                    routine.status = "Paused".into();
                    for run in r
                        .runs
                        .values_mut()
                        .filter(|run| run.routine_id == id && run.status == "Queued")
                    {
                        run.status = "Cancelled".into();
                        run.updated_at = chrono::Utc::now().to_rfc3339();
                    }
                    Ok(())
                })
                .await?;
        }
        _ => return Err(anyhow::anyhow!("Unknown routine action").into()),
    }
    Ok(Json(app.runtime.store.read().await.routines[&id].clone()))
}
async fn results(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Value> {
    let handle = app
        .runtime
        .live
        .lock()
        .await
        .get(&id)
        .context("Activate or preview the query to load results")?
        .handle
        .clone();
    Ok(Json(handle.snapshot().await?))
}

#[derive(Deserialize)]
struct Page {
    limit: Option<usize>,
    before: Option<String>,
}
async fn runs(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Json<Value> {
    let mut runs: Vec<_> = app
        .runtime
        .store
        .read()
        .await
        .runs
        .into_values()
        .filter(|r| r.routine_id == id)
        .collect();
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    let start = page
        .before
        .as_ref()
        .and_then(|id| runs.iter().position(|r| &r.id == id))
        .map_or(0, |i| i + 1);
    Json(json!(runs
        .into_iter()
        .skip(start)
        .take(page.limit.unwrap_or(50).min(100))
        .collect::<Vec<_>>()))
}
async fn run(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!(app
        .runtime
        .store
        .read()
        .await
        .runs
        .get(&id)
        .context("Run not found")?)))
}

async fn preview(
    State(app): State<Arc<App>>,
    Json(mut routine): Json<Routine>,
) -> ApiResult<Value> {
    let _operation = app.operations.lock().await;
    validate_query_definition(&routine)?;
    let id = format!("preview-{}", uuid::Uuid::new_v4());
    routine.id = id.clone();
    routine.name = "Query preview".into();
    routine.agent = "preview".into();
    routine.prompt = "Preview only".into();
    if routine.key_fields.is_empty() {
        routine.key_fields = vec!["preview".into()];
    }
    if routine.operations.is_empty() {
        routine.operations = vec!["added".into()];
    }
    require(
        app.previews.lock().await.len() < 5,
        "Close an existing preview first",
    )?;
    app.runtime
        .start(&routine, crate::runtime::StartMode::Preview)
        .await?;
    app.previews.lock().await.insert(id.clone(), id.clone());
    let cleanup_app = app.clone();
    let cleanup_id = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        if cleanup_app
            .previews
            .lock()
            .await
            .remove(&cleanup_id)
            .is_some()
        {
            if let Err(e) = cleanup_app.runtime.remove(&cleanup_id).await {
                log::error!("Preview cleanup failed: {e:#}");
            }
        }
    });
    Ok(Json(json!({"id":id})))
}
async fn preview_results(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Value> {
    results(State(app), Path(id)).await
}
async fn close_preview(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Value> {
    let _operation = app.operations.lock().await;
    require(
        app.previews.lock().await.remove(&id).is_some(),
        "Preview not found",
    )?;
    app.runtime.remove(&id).await?;
    Ok(Json(json!({"ok":true})))
}

async fn demo_actions(State(app): State<Arc<App>>) -> ApiResult<Value> {
    require(app.fixtures, "Fixture controls are disabled")?;
    let actions = crate::fixtures::actions()?;
    Ok(Json(json!(actions
        .as_array()
        .context("Invalid fixture actions")?
        .iter()
        .map(|a| json!({"name":a["name"],"title":a["title"]}))
        .collect::<Vec<_>>())))
}
async fn demo_action(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Value> {
    require(app.fixtures, "Fixture controls are disabled")?;
    crate::fixtures::execute(&app.runtime.store, &id).await?;
    Ok(Json(json!({"ok":true})))
}

async fn events(
    State(app): State<Arc<App>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let receiver = app.runtime.store.events.subscribe();
    let shutdown = app.shutdown.subscribe();
    let stream =
        futures_util::stream::unfold((receiver, shutdown), |(mut receiver, mut shutdown)| async {
            if *shutdown.borrow() {
                return None;
            }
            tokio::select! {
                _=shutdown.changed()=>None,
                result=receiver.recv()=>match result {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some((
                    Ok(Event::default()
                        .event("refresh")
                        .data("{\"type\":\"refresh\"}")),
                    (receiver,shutdown),
                )),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
                }
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
