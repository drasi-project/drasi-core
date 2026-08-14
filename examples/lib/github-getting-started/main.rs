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

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use drasi_lib::wal::CapacityPolicy;
use drasi_lib::{DrasiLib, DurabilityConfig, Query};
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
use drasi_source_github::config::{GitHubSourceConfig, WebhookConfig};
use drasi_source_github::GitHubSourceBuilder;
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
struct MockGitHubState {
    issue_exists: Arc<RwLock<bool>>,
    issue_title: Arc<RwLock<String>>,
}

#[derive(Debug, Deserialize)]
struct IssueControlRequest {
    exists: Option<bool>,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct IssueControlResponse {
    exists: bool,
    title: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mock_addr = std::env::var("GITHUB_EXAMPLE_GRAPHQL_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:19080".to_string());
    let webhook_host =
        std::env::var("GITHUB_EXAMPLE_WEBHOOK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let webhook_port: u16 = std::env::var("GITHUB_EXAMPLE_WEBHOOK_PORT")
        .unwrap_or_else(|_| "19081".to_string())
        .parse()
        .context("Invalid GITHUB_EXAMPLE_WEBHOOK_PORT")?;
    let webhook_path =
        std::env::var("GITHUB_EXAMPLE_WEBHOOK_PATH").unwrap_or_else(|_| "/webhook".to_string());
    let webhook_secret = std::env::var("GITHUB_EXAMPLE_WEBHOOK_SECRET")
        .unwrap_or_else(|_| "example-secret".to_string());
    let graphql_url = std::env::var("GITHUB_EXAMPLE_GRAPHQL_URL")
        .unwrap_or_else(|_| format!("http://{mock_addr}/graphql"));
    let data_dir = std::env::var("GITHUB_EXAMPLE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".data"));

    std::fs::create_dir_all(data_dir.join("wal")).context("Failed to create WAL directory")?;
    std::fs::create_dir_all(&data_dir).context("Failed to create data directory")?;

    let state = MockGitHubState {
        issue_exists: Arc::new(RwLock::new(false)),
        issue_title: Arc::new(RwLock::new("Issue created from webhook".to_string())),
    };

    let mock_listener = tokio::net::TcpListener::bind(&mock_addr)
        .await
        .with_context(|| format!("Failed to bind mock GitHub server at {mock_addr}"))?;
    let mock_app = Router::new()
        .route("/graphql", post(mock_graphql_handler))
        .route("/healthz", get(healthz_handler))
        .route("/control/issue", get(get_issue_control).post(post_issue_control))
        .with_state(state.clone());
    let mock_server = tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_app).await;
    });

    let source = GitHubSourceBuilder::new("github-source")
        .with_config(GitHubSourceConfig {
            token: "example-token".to_string(),
            repositories: vec!["acme/repo".to_string()],
            projects: Vec::new(),
            webhook: WebhookConfig {
                host: webhook_host.clone(),
                port: webhook_port,
                path: webhook_path.clone(),
                secret: webhook_secret.clone(),
                body_limit_bytes: 1024 * 1024,
            },
            reconcile_interval_secs: 300,
            durability: DurabilityConfig {
                enabled: true,
                max_events: 1024,
                capacity_policy: CapacityPolicy::RejectIncoming,
            },
            graphql_url: graphql_url.clone(),
            skip_initial_bootstrap: true,
        })
        .build()
        .context("Failed to build GitHub source")?;

    let query = Query::cypher("github-issues")
        .query(
            r#"
            MATCH (i:GitHubIssue)
            RETURN i.number AS issue_number,
                   i.title AS title,
                   i.state AS state,
                   i.repositoryNameWithOwner AS repository
        "#,
        )
        .from_source("github-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let reaction_template = QueryConfig {
        added: Some(TemplateSpec::new(
            "➕ ISSUE INSERTED: #{{after.issue_number}} {{after.title}} ({{after.state}}) [{{after.repository}}]",
        )),
        updated: Some(TemplateSpec::new(
            "🔄 ISSUE UPDATED: #{{after.issue_number}} {{before.title}} -> {{after.title}} ({{after.state}})",
        )),
        deleted: Some(TemplateSpec::new(
            "➖ ISSUE DELETED: #{{before.issue_number}} {{before.title}}",
        )),
    };
    let reaction = LogReaction::builder("github-log")
        .from_query("github-issues")
        .with_default_template(reaction_template)
        .build()
        .context("Failed to build log reaction")?;

    let wal = Arc::new(RedbWalProvider::new(data_dir.join("wal")));
    let state_store = Arc::new(
        RedbStateStoreProvider::new(data_dir.join("state.redb"))
            .context("Failed to create Redb state store")?,
    );

    let core = DrasiLib::builder()
        .with_id("github-getting-started")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .with_wal_provider(wal)
        .with_state_store_provider(state_store)
        .build()
        .await
        .context("Failed to build DrasiLib")?;

    core.start().await.context("Failed to start DrasiLib")?;

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║      GitHub Source Getting Started (Local Harness)      ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("Mock GitHub API server : http://{mock_addr}");
    println!("Webhook listener       : http://{webhook_host}:{webhook_port}{webhook_path}");
    println!("Webhook health         : http://{webhook_host}:{webhook_port}/health");
    println!("Control endpoint       : http://{mock_addr}/control/issue");
    println!();
    println!("Run in another shell:");
    println!("  ./test-updates.sh");
    println!();
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    core.stop().await.context("Failed to stop DrasiLib")?;
    mock_server.abort();
    Ok(())
}

async fn healthz_handler() -> StatusCode {
    StatusCode::OK
}

async fn get_issue_control(State(state): State<MockGitHubState>) -> Json<IssueControlResponse> {
    Json(IssueControlResponse {
        exists: *state.issue_exists.read().await,
        title: state.issue_title.read().await.clone(),
    })
}

async fn post_issue_control(
    State(state): State<MockGitHubState>,
    Json(request): Json<IssueControlRequest>,
) -> Json<IssueControlResponse> {
    if let Some(exists) = request.exists {
        *state.issue_exists.write().await = exists;
    }
    if let Some(title) = request.title {
        *state.issue_title.write().await = title;
    }
    get_issue_control(State(state)).await
}

async fn mock_graphql_handler(
    State(state): State<MockGitHubState>,
    Json(payload): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let query = payload
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let variables = payload.get("variables").cloned().unwrap_or(json!({}));
    let node_id = variables
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if query.contains("... on Issue") && node_id == "I_1" {
        if !*state.issue_exists.read().await {
            return (StatusCode::OK, Json(json!({ "data": { "node": null } })));
        }
        let title = state.issue_title.read().await.clone();
        return (
            StatusCode::OK,
            Json(json!({
                "data": {
                    "node": {
                        "__typename": "Issue",
                        "id": "I_1",
                        "number": 1,
                        "title": title,
                        "body": "Issue body",
                        "state": "OPEN",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "closedAt": null,
                        "url": "https://github.com/acme/repo/issues/1",
                        "author": { "login": "octocat" },
                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                        "assignees": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        },
                        "labels": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        },
                        "comments": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        }
                    }
                }
            })),
        );
    }

    (StatusCode::OK, Json(json!({ "data": { "node": null } })))
}
