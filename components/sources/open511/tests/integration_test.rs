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

//! Integration test with a local Open511 protocol harness.

#![allow(clippy::unwrap_used)]

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post, put},
    Json, Router,
};
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query as DrasiQuery};
use drasi_reaction_application::subscription::{Subscription, SubscriptionOptions};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_open511::models::{
    Open511Area, Open511Event, Open511EventsResponse, Open511Meta, Open511Pagination, Open511Road,
};
use drasi_source_open511::Open511Source;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout, Instant};

#[derive(Clone)]
struct MockServerState {
    events: Arc<RwLock<HashMap<String, Open511Event>>>,
}

/// Bind to an ephemeral port and start the mock server on it, returning the port.
///
/// The listener is created once and handed directly to `axum::serve` so the
/// OS-assigned port remains reserved — no TOCTOU race.
async fn start_mock_open511_server(initial_events: Vec<Open511Event>) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind mock Open511 server")?;
    let port = listener.local_addr()?.port();

    let state = MockServerState {
        events: Arc::new(RwLock::new(
            initial_events
                .into_iter()
                .map(|event| (event.id.clone(), event))
                .collect(),
        )),
    };

    let app = Router::new()
        .route("/events", get(handle_events))
        .route("/admin/events", post(admin_insert_event))
        .route("/admin/events/:event_id", put(admin_upsert_event))
        .route("/admin/events/:event_id", delete(admin_delete_event))
        .with_state(state);

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("mock Open511 server failed: {e}");
        }
    });

    Ok(port)
}

async fn handle_events(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<MockServerState>,
) -> Json<Open511EventsResponse> {
    let mut events: Vec<Open511Event> = state.events.read().await.values().cloned().collect();

    if let Some(status) = params.get("status") {
        events.retain(|event| event.status == *status);
    }

    if let Some(updated_filter) = params.get("updated") {
        if let Some(ts) = updated_filter.strip_prefix('>') {
            if let Ok(since) = chrono::DateTime::parse_from_rfc3339(ts) {
                events.retain(|event| {
                    event.updated_or_created().is_some_and(|candidate| {
                        chrono::DateTime::parse_from_rfc3339(candidate)
                            .map(|dt| dt > since)
                            .unwrap_or(false)
                    })
                });
            }
        }
    }

    events.sort_by(|left, right| left.id.cmp(&right.id));

    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50);
    let offset = params
        .get("offset")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    let paged = events
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    Json(Open511EventsResponse {
        events: paged,
        pagination: Some(Open511Pagination {
            offset: Some(json!(offset.to_string())),
            next_url: None,
        }),
        meta: Some(Open511Meta {
            url: Some("/events".to_string()),
            version: Some("v1".to_string()),
            up_url: Some("/".to_string()),
        }),
    })
}

async fn admin_insert_event(
    State(state): State<MockServerState>,
    Json(event): Json<Open511Event>,
) -> StatusCode {
    state.events.write().await.insert(event.id.clone(), event);
    StatusCode::OK
}

async fn admin_upsert_event(
    Path(event_id): Path<String>,
    State(state): State<MockServerState>,
    Json(mut event): Json<Open511Event>,
) -> StatusCode {
    event.id = event_id.clone();
    state.events.write().await.insert(event_id, event);
    StatusCode::OK
}

async fn admin_delete_event(
    Path(event_id): Path<String>,
    State(state): State<MockServerState>,
) -> StatusCode {
    state.events.write().await.remove(&event_id);
    StatusCode::OK
}

fn make_event(id: &str, severity: &str, updated: &str) -> Open511Event {
    Open511Event {
        id: id.to_string(),
        status: "ACTIVE".to_string(),
        headline: Some("INCIDENT".to_string()),
        event_type: Some("INCIDENT".to_string()),
        severity: Some(severity.to_string()),
        updated: Some(updated.to_string()),
        roads: Some(vec![Open511Road {
            name: Some("Highway 1".to_string()),
            from: Some("A".to_string()),
            to: Some("B".to_string()),
            direction: Some("BOTH".to_string()),
            state: Some("CLOSED".to_string()),
            delay: None,
        }]),
        areas: Some(vec![Open511Area {
            id: Some("drivebc.ca/3".to_string()),
            name: Some("Rocky Mountain District".to_string()),
            url: None,
        }]),
        ..Default::default()
    }
}

async fn wait_for_diff<F>(
    subscription: &mut Subscription,
    max_wait: Duration,
    matcher: F,
) -> Result<ResultDiff>
where
    F: Fn(&ResultDiff) -> bool,
{
    let deadline = Instant::now() + max_wait;

    loop {
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!("timed out waiting for expected query diff");
        }
        let remaining = deadline.saturating_duration_since(now);

        match timeout(remaining, subscription.recv()).await {
            Ok(Some(result)) => {
                for diff in result.results {
                    if matcher(&diff) {
                        return Ok(diff);
                    }
                }
            }
            Ok(None) => continue,
            Err(_) => anyhow::bail!("timed out waiting for subscription result"),
        }
    }
}

#[tokio::test]
#[ignore] // Run with: cargo test -p drasi-source-open511 --test integration_test -- --ignored --nocapture
async fn test_open511_change_detection_with_client_harness() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    timeout(Duration::from_secs(30), async {
        let port = start_mock_open511_server(vec![
            make_event("test-EVT-001", "MAJOR", "2026-03-08T01:00:00Z"),
            make_event("test-EVT-002", "MAJOR", "2026-03-08T01:00:00Z"),
        ])
        .await
        .context("failed to start mock open511 server")?;

        let source = Open511Source::builder("road-events")
            .with_base_url(format!("http://127.0.0.1:{port}"))
            .with_poll_interval_secs(1)
            .with_full_sweep_interval(2)
            .with_status_filter("ACTIVE")
            .with_start_from_beginning()
            .build()
            .context("failed to build Open511 source")?;

        let query = DrasiQuery::cypher("road-events-query")
            .query(
                r#"
                MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
                RETURN e.id AS event_id, e.severity AS severity, r.name AS road_name
            "#,
            )
            .from_source("road-events")
            .auto_start(true)
            .build();

        let (reaction, handle) = ApplicationReactionBuilder::new("test-reaction")
            .with_query("road-events-query")
            .build();

        let core = DrasiLib::builder()
            .with_id("open511-integration-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .context("failed to build DrasiLib")?;

        core.start().await.context("failed to start DrasiLib")?;

        let mut subscription = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(10)),
            )
            .await
            .context("failed to create subscription")?;

        // Drain bootstrap/initial state diffs.
        sleep(Duration::from_secs(2)).await;
        while timeout(Duration::from_millis(100), subscription.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {}

        let client = reqwest::Client::new();

        // CREATE / INSERT
        let insert_event = make_event("test-EVT-003", "MAJOR", "2026-03-08T01:05:00Z");
        let insert_response = client
            .post(format!("http://127.0.0.1:{port}/admin/events"))
            .json(&insert_event)
            .send()
            .await?;
        assert_eq!(insert_response.status(), reqwest::StatusCode::OK);

        let _ = wait_for_diff(
            &mut subscription,
            Duration::from_secs(10),
            |diff| match diff {
                ResultDiff::Add { data } => data["event_id"] == "test-EVT-003",
                _ => false,
            },
        )
        .await
        .context("did not observe INSERT")?;

        // UPDATE / MODIFY
        let update_event = make_event("test-EVT-001", "MINOR", "2026-03-08T01:10:00Z");
        let update_response = client
            .put(format!("http://127.0.0.1:{port}/admin/events/test-EVT-001"))
            .json(&update_event)
            .send()
            .await?;
        assert_eq!(update_response.status(), reqwest::StatusCode::OK);

        let _ = wait_for_diff(
            &mut subscription,
            Duration::from_secs(10),
            |diff| match diff {
                ResultDiff::Update { after, .. } => {
                    after["event_id"] == "test-EVT-001" && after["severity"] == "MINOR"
                }
                ResultDiff::Add { data } => {
                    data["event_id"] == "test-EVT-001" && data["severity"] == "MINOR"
                }
                _ => false,
            },
        )
        .await
        .context("did not observe UPDATE")?;

        // DELETE / REMOVE (requires full sweep cycle)
        let delete_response = client
            .delete(format!("http://127.0.0.1:{port}/admin/events/test-EVT-002"))
            .send()
            .await?;
        assert_eq!(delete_response.status(), reqwest::StatusCode::OK);

        let _ = wait_for_diff(
            &mut subscription,
            Duration::from_secs(12),
            |diff| match diff {
                ResultDiff::Delete { data } => data["event_id"] == "test-EVT-002",
                _ => false,
            },
        )
        .await
        .context("did not observe DELETE")?;

        core.stop().await.context("failed to stop DrasiLib")?;

        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("integration test timed out")??;

    Ok(())
}
