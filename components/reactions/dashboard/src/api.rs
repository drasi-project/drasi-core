// Copyright 2026 The Drasi Authors.
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

//! REST API handlers for dashboard CRUD and query metadata.

use crate::storage::{DashboardConfig, DashboardStorage, DashboardWidget, GridOptions};
use crate::websocket::{QuerySnapshot, QuerySnapshotStore};
use anyhow::anyhow;
use axum::{
    body::{to_bytes, Body},
    extract::{Path, State},
    http::{Request, StatusCode},
    routing::get,
    Json, Router,
};
use drasi_lib::StateStoreProvider;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower::ServiceExt;

/// Shared state for dashboard API handlers.
#[derive(Clone)]
pub struct ApiState {
    reaction_id: String,
    query_ids: Arc<Vec<String>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    snapshot_store: QuerySnapshotStore,
    results_api_url: Option<String>,
    results_api_client: Option<reqwest::Client>,
}

impl ApiState {
    pub fn new(
        reaction_id: impl Into<String>,
        query_ids: Vec<String>,
        state_store: Option<Arc<dyn StateStoreProvider>>,
        snapshot_store: QuerySnapshotStore,
    ) -> Self {
        Self {
            reaction_id: reaction_id.into(),
            query_ids: Arc::new(query_ids),
            state_store,
            snapshot_store,
            results_api_url: None,
            results_api_client: None,
        }
    }

    pub fn with_results_api_url(mut self, url: Option<String>) -> Self {
        if url.is_some() {
            self.results_api_client = Some(
                reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .expect("failed to build reqwest client"),
            );
        }
        self.results_api_url = url;
        self
    }

    fn storage(&self) -> anyhow::Result<DashboardStorage> {
        let Some(state_store) = &self.state_store else {
            return Err(anyhow!("dashboard API state store is not configured"));
        };
        Ok(DashboardStorage::new(
            self.reaction_id.clone(),
            Arc::clone(state_store),
        ))
    }

    fn query_ids(&self) -> Vec<String> {
        self.query_ids.as_ref().clone()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateDashboardRequest {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    grid_options: Option<GridOptions>,
    #[serde(default)]
    widgets: Vec<DashboardWidget>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateDashboardRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    grid_options: Option<GridOptions>,
    #[serde(default)]
    widgets: Option<Vec<DashboardWidget>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueryListResponse {
    query_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

type ApiError = (StatusCode, Json<ErrorResponse>);
type ApiResult<T> = Result<T, ApiError>;

fn bad_request(message: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

fn not_found(message: impl Into<String>) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

fn internal_error(error: anyhow::Error) -> ApiError {
    log::error!("dashboard API internal error: {error:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "internal server error".to_string(),
        }),
    )
}

/// Build the dashboard API router.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/api/dashboards",
            get(list_dashboards).post(create_dashboard),
        )
        .route(
            "/api/dashboards/:id",
            get(get_dashboard)
                .put(update_dashboard)
                .delete(delete_dashboard),
        )
        .route("/api/queries", get(list_queries))
        .route("/api/queries/:id/snapshot", get(get_query_snapshot))
}

async fn list_dashboards(State(state): State<ApiState>) -> ApiResult<Json<Vec<DashboardConfig>>> {
    let storage = state.storage().map_err(internal_error)?;
    let dashboards = storage.list_dashboards().await.map_err(internal_error)?;
    Ok(Json(dashboards))
}

async fn get_dashboard(
    State(state): State<ApiState>,
    Path(dashboard_id): Path<String>,
) -> ApiResult<Json<DashboardConfig>> {
    let storage = state.storage().map_err(internal_error)?;
    let Some(dashboard) = storage
        .get_dashboard(&dashboard_id)
        .await
        .map_err(internal_error)?
    else {
        return Err(not_found(format!(
            "dashboard '{dashboard_id}' was not found"
        )));
    };
    Ok(Json(dashboard))
}

async fn create_dashboard(
    State(state): State<ApiState>,
    Json(payload): Json<CreateDashboardRequest>,
) -> ApiResult<(StatusCode, Json<DashboardConfig>)> {
    if payload.name.trim().is_empty() {
        return Err(bad_request("dashboard name cannot be empty"));
    }

    let storage = state.storage().map_err(internal_error)?;
    let mut dashboard = DashboardConfig::new(
        payload.name.trim().to_string(),
        payload.grid_options.unwrap_or_default(),
        payload.widgets,
    );
    if let Some(id) = payload.id.filter(|value| !value.trim().is_empty()) {
        dashboard.id = id;
    }
    let saved = storage
        .save_dashboard(dashboard)
        .await
        .map_err(internal_error)?;

    Ok((StatusCode::CREATED, Json(saved)))
}

async fn update_dashboard(
    State(state): State<ApiState>,
    Path(dashboard_id): Path<String>,
    Json(payload): Json<UpdateDashboardRequest>,
) -> ApiResult<Json<DashboardConfig>> {
    let storage = state.storage().map_err(internal_error)?;
    let Some(mut dashboard) = storage
        .get_dashboard(&dashboard_id)
        .await
        .map_err(internal_error)?
    else {
        return Err(not_found(format!(
            "dashboard '{dashboard_id}' was not found"
        )));
    };

    if let Some(name) = payload.name {
        if name.trim().is_empty() {
            return Err(bad_request("dashboard name cannot be empty"));
        }
        dashboard.name = name.trim().to_string();
    }
    if let Some(grid_options) = payload.grid_options {
        dashboard.grid_options = grid_options;
    }
    if let Some(widgets) = payload.widgets {
        dashboard.widgets = widgets;
    }

    let saved = storage
        .save_dashboard(dashboard)
        .await
        .map_err(internal_error)?;
    Ok(Json(saved))
}

async fn delete_dashboard(
    State(state): State<ApiState>,
    Path(dashboard_id): Path<String>,
) -> ApiResult<StatusCode> {
    let storage = state.storage().map_err(internal_error)?;
    let removed = storage
        .delete_dashboard(&dashboard_id)
        .await
        .map_err(internal_error)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found(format!(
            "dashboard '{dashboard_id}' was not found"
        )))
    }
}

async fn list_queries(State(state): State<ApiState>) -> ApiResult<Json<QueryListResponse>> {
    Ok(Json(QueryListResponse {
        query_ids: state.query_ids(),
    }))
}

async fn get_query_snapshot(
    State(state): State<ApiState>,
    Path(query_id): Path<String>,
) -> ApiResult<Json<QuerySnapshot>> {
    // Validate query_id against the known query list to prevent SSRF.
    if !state.query_ids.contains(&query_id) {
        return Err(not_found(format!("query '{query_id}' not found")));
    }

    let snapshot = state.snapshot_store.get_snapshot(&query_id).await;
    if !snapshot.rows.is_empty() || snapshot.aggregation.is_some() {
        return Ok(Json(snapshot));
    }

    // Fall back to the results API if configured and local snapshot is empty.
    if let (Some(base_url), Some(client)) = (&state.results_api_url, &state.results_api_client) {
        let url = format!(
            "{}/queries/{}/results",
            base_url.trim_end_matches('/'),
            query_id
        );
        if let Ok(response) = client.get(&url).send().await {
            if let Ok(rows) = response.json::<Vec<serde_json::Value>>().await {
                return Ok(Json(QuerySnapshot {
                    rows,
                    aggregation: None,
                }));
            }
        }
    }

    Ok(Json(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::{MemoryStateStoreProvider, StateStoreProvider};

    fn test_state() -> ApiState {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        ApiState::new(
            "test-reaction",
            vec!["query-1".to_string()],
            Some(store),
            QuerySnapshotStore::new(),
        )
    }

    #[tokio::test]
    async fn test_dashboard_crud_routes() {
        let app = router().with_state(test_state());

        let create_request = Request::builder()
            .method("POST")
            .uri("/api/dashboards")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{
                    "name": "test dashboard",
                    "widgets": [],
                    "gridOptions": {"columns": 12, "rowHeight": 60, "margin": 10}
                }"#,
            ))
            .expect("request should build");

        let create_response = app
            .clone()
            .oneshot(create_request)
            .await
            .expect("request should execute");
        assert_eq!(create_response.status(), StatusCode::CREATED);

        let created_bytes = to_bytes(create_response.into_body(), 1024 * 1024)
            .await
            .expect("response body should read");
        let created_dashboard: DashboardConfig =
            serde_json::from_slice(&created_bytes).expect("response should be dashboard json");

        let get_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/dashboards/{}", created_dashboard.id))
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should execute");
        assert_eq!(get_response.status(), StatusCode::OK);

        let update_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/dashboards/{}", created_dashboard.id))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"updated dashboard"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("request should execute");
        assert_eq!(update_response.status(), StatusCode::OK);

        let delete_response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/dashboards/{}", created_dashboard.id))
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should execute");
        assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_queries_route() {
        let app = router().with_state(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/queries")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should execute");

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body should read");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("response should be valid json");
        assert_eq!(body["queryIds"][0], "query-1");
    }
}
