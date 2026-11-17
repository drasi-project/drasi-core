// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0
use anyhow::{ensure, Result};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use drasi_lib::channels::{QueryResult, ResultDiff};
use foundry_routines_demo::{
    api::App,
    fixtures,
    foundry::Foundry,
    reaction::{admit, FoundryReaction},
    runtime::Runtime,
    store::Store,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn setup() -> Result<(tempfile::TempDir, Arc<Store>, Arc<Runtime>)> {
    let dir = tempfile::tempdir()?;
    let provider = Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(
        dir.path().join("state.redb"),
    )?);
    let store = Store::open(provider).await?;
    fixtures::load(&store, true).await?;
    let runtime = Runtime::new(store.clone()).await?;
    Ok((dir, store, runtime))
}

#[tokio::test]
async fn typed_diffs_are_deduplicated_by_emission_and_diff_index() -> Result<()> {
    let (_dir, store, runtime) = setup().await?;
    let routine = store.read().await.routines["order-risk"].clone();
    let reaction = FoundryReaction::new(
        "reaction".into(),
        "query".into(),
        routine.clone(),
        store.clone(),
        true,
    );
    let event = QueryResult::new(
        "query".into(),
        u64::MAX,
        chrono::Utc::now(),
        vec![
            ResultDiff::Add {
                data: json!({"order_id":"same-key","required":5}),
                row_signature: u64::MAX,
            },
            ResultDiff::Add {
                data: json!({"order_id":"same-key","required":6}),
                row_signature: u64::MAX,
            },
        ],
        Default::default(),
    );
    admit(&store, &routine, &reaction.handle, &event).await?;
    admit(&store, &routine, &reaction.handle, &event).await?;
    let runs = store.read().await.runs;
    ensure!(
        runs.len() == 2,
        "Distinct diffs should have distinct jobs; replay must not duplicate them"
    );
    let sequence = u64::MAX.to_string();
    ensure!(
        runs.values()
            .all(|run| run.input["event"]["sequence"].as_str() == Some(sequence.as_str())),
        "Browser integer precision must be preserved"
    );
    reaction.handle.gate.lock().await.cutoff = u64::MAX;
    admit(&store, &routine, &reaction.handle, &event).await?;
    ensure!(store.read().await.runs.len() == 2);
    runtime.core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_result_key_is_visible_but_cannot_dispatch() -> Result<()> {
    let (_dir, store, runtime) = setup().await?;
    let routine = store.read().await.routines["order-risk"].clone();
    let reaction = FoundryReaction::new(
        "reaction".into(),
        "query".into(),
        routine.clone(),
        store.clone(),
        true,
    );
    let event = QueryResult::new(
        "query".into(),
        1,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: json!({"wrong_key":"value"}),
            row_signature: 5,
        }],
        Default::default(),
    );
    admit(&store, &routine, &reaction.handle, &event).await?;
    ensure!(store
        .read()
        .await
        .runs
        .values()
        .all(|r| r.status == "Failed" && r.error.is_some()));
    runtime.core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn creation_assigns_ids_and_management_rejects_foreign_origins_and_conflicts() -> Result<()> {
    let (_dir, store, runtime) = setup().await?;
    let app = App::new(runtime.clone(), Foundry::new(true)?, false, 8780);
    let mut routine = store.read().await.routines["order-risk"].clone();
    let request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("host", "127.0.0.1:8780")
        .header("origin", "https://foreign.example")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&routine)?))?;
    ensure!(App::router(app.clone()).oneshot(request).await?.status() == StatusCode::FORBIDDEN);
    routine.revision = 99;
    let request = Request::builder()
        .method("PUT")
        .uri("/api/routines/order-risk")
        .header("host", "127.0.0.1:8780")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&routine)?))?;
    ensure!(App::router(app.clone()).oneshot(request).await?.status() == StatusCode::CONFLICT);
    routine.id.clear();
    routine.revision = 0;
    let request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("host", "127.0.0.1:8780")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&routine)?))?;
    let response = App::router(app).oneshot(request).await?;
    ensure!(
        response.status().is_success(),
        "The UI must be able to save an unsaved routine without an ID"
    );
    let body = axum::body::to_bytes(response.into_body(), 65536).await?;
    let saved: Value = serde_json::from_slice(&body)?;
    ensure!(
        saved["id"]
            .as_str()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            && saved["status"] == "Draft"
    );
    runtime.core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn restart_preserves_history_without_resubmitting_unknown_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let provider = Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(
        dir.path().join("state.redb"),
    )?);
    let store = Store::open(provider.clone()).await?;
    fixtures::load(&store, true).await?;
    let routine = store.read().await.routines["order-risk"].clone();
    let reaction = FoundryReaction::new(
        "reaction".into(),
        "query".into(),
        routine.clone(),
        store.clone(),
        true,
    );
    let event = QueryResult::new(
        "query".into(),
        1,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: json!({"order_id":"id"}),
            row_signature: 5,
        }],
        Default::default(),
    );
    admit(&store, &routine, &reaction.handle, &event).await?;
    store
        .update(|records| {
            records.routines.get_mut("order-risk").unwrap().status = "Active".into();
            records.runs.values_mut().next().unwrap().status = "Submitting".into();
            Ok(())
        })
        .await?;
    drop(reaction);
    drop(store);
    let recovered = Store::open(provider).await?;
    ensure!(recovered.read().await.routines["order-risk"].status == "Interrupted");
    ensure!(recovered
        .read()
        .await
        .runs
        .values()
        .all(|r| r.status == "Unknown"));
    let serialized: Value = serde_json::to_value(recovered.read().await.connections)?;
    ensure!(
        !serialized.to_string().contains("local-fixture-only"),
        "Passwords must not be persisted"
    );
    Ok(())
}

#[tokio::test]
async fn shutdown_closes_open_event_streams() -> Result<()> {
    let (_dir, _store, runtime) = setup().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = App::new(runtime.clone(), Foundry::new(true)?, false, address.port());
    let mut shutdown = app.shutdown.subscribe();
    let server = tokio::spawn(async move {
        axum::serve(listener, App::router(app))
            .with_graceful_shutdown(async move {
                let _ = shutdown.changed().await;
            })
            .await
    });
    let client = reqwest::Client::new();
    let _stream = client
        .get(format!("http://{address}/api/events"))
        .send()
        .await?;
    ensure!(client
        .post(format!("http://{address}/api/shutdown"))
        .send()
        .await?
        .status()
        .is_success());
    tokio::time::timeout(std::time::Duration::from_secs(3), server).await???;
    runtime.core.stop().await?;
    Ok(())
}
