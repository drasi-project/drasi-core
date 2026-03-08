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
use axum::{extract::State, routing::post, Json, Router};
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::{Subscription, SubscriptionOptions};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_sui_deepbook::SuiDeepBookSource;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const SOURCE_ID: &str = "deepbook-source";
const QUERY_ID: &str = "deepbook-query";

fn diff_matches(diff: &ResultDiff, expected_kind: &str, expected_id: &str) -> bool {
    match (expected_kind, diff) {
        ("ADD", ResultDiff::Add { data }) => data
            .get("entity_id")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == expected_id),
        ("DELETE", ResultDiff::Delete { data }) => data
            .get("entity_id")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == expected_id),
        _ => false,
    }
}

fn update_matches(diff: &ResultDiff, expected_id: &str) -> bool {
    match diff {
        ResultDiff::Update { before, after, .. } => {
            before
                .get("entity_id")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v == expected_id)
                && after
                    .get("entity_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| v == expected_id)
        }
        _ => false,
    }
}

/// Waits for a matching diff, collecting all observed entity_ids into `seen_ids`.
async fn wait_for_change<F>(
    subscription: &mut Subscription,
    attempts: usize,
    seen_ids: &mut Vec<String>,
    mut matcher: F,
) -> Result<ResultDiff>
where
    F: FnMut(&ResultDiff) -> bool,
{
    for _ in 0..attempts {
        if let Some(result) = subscription.recv().await {
            for entry in &result.results {
                if let Some(id) = extract_entity_id(entry) {
                    seen_ids.push(id);
                }
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }
    anyhow::bail!("Timed out waiting for expected query diff")
}

fn extract_entity_id(diff: &ResultDiff) -> Option<String> {
    let data = match diff {
        ResultDiff::Add { data } => Some(data),
        ResultDiff::Delete { data } => Some(data),
        ResultDiff::Update { after, .. } => Some(after),
        _ => None,
    };
    data.and_then(|d| d.get("entity_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[derive(Clone)]
struct MockState {
    call_count: Arc<Mutex<u32>>,
}

async fn mock_sui_rpc(State(state): State<MockState>, Json(request): Json<Value>) -> Json<Value> {
    let mut call_count = state.call_count.lock().await;
    let response_id = request
        .get("id")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(1));

    let payload = match *call_count {
        0 => serde_json::json!({
            "data": [
                mock_event_with_pkg("0", "0xother-pkg", "TokenTransfer", serde_json::json!({
                    "amount": "500"
                })),
                mock_event_with_pkg("1", "0xdeepbook", "OrderPlaced", serde_json::json!({
                    "order_id": "42",
                    "pool_id": "pool-alpha",
                    "size": "1000",
                    "price": "2.34"
                }))
            ],
            "nextCursor": {"txDigest": "0xtx1", "eventSeq": "1"},
            "hasNextPage": false
        }),
        1 => serde_json::json!({
            "data": [
                mock_event_with_pkg("2", "0xother-pkg", "SomethingElse", serde_json::json!({
                    "foo": "bar"
                })),
                mock_event_with_pkg("3", "0xdeepbook", "OrderUpdated", serde_json::json!({
                    "order_id": "42",
                    "pool_id": "pool-alpha",
                    "size": "1200",
                    "price": "2.35"
                }))
            ],
            "nextCursor": {"txDigest": "0xtx3", "eventSeq": "3"},
            "hasNextPage": false
        }),
        2 => serde_json::json!({
            "data": [mock_event_with_pkg("4", "0xdeepbook", "OrderCancelled", serde_json::json!({
                "order_id": "42",
                "pool_id": "pool-alpha"
            }))],
            "nextCursor": {"txDigest": "0xtx4", "eventSeq": "4"},
            "hasNextPage": false
        }),
        _ => serde_json::json!({
            "data": [],
            "nextCursor": null,
            "hasNextPage": false
        }),
    };

    *call_count += 1;

    Json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": response_id,
        "result": payload
    }))
}

fn mock_event_with_pkg(
    event_seq: &str,
    package_id: &str,
    event_suffix: &str,
    parsed_json: Value,
) -> Value {
    serde_json::json!({
        "id": {
            "txDigest": format!("0xtx{event_seq}"),
            "eventSeq": event_seq,
        },
        "packageId": package_id,
        "transactionModule": "pool",
        "sender": "0xsender",
        "type": format!("{package_id}::events::{event_suffix}"),
        "parsedJson": parsed_json,
        "timestampMs": "1772923888171",
    })
}

#[tokio::test]
#[ignore] // Run with: cargo test -p drasi-source-sui-deepbook --test integration_test -- --ignored --nocapture
async fn test_sui_deepbook_change_detection_end_to_end() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind mock Sui RPC listener")?;
    let address = listener.local_addr()?;

    let mock_state = MockState {
        call_count: Arc::new(Mutex::new(0)),
    };
    let app = Router::new()
        .route("/", post(mock_sui_rpc))
        .with_state(mock_state.clone());

    let server_handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("Mock Sui RPC server failed: {err}");
        }
    });

    let source = SuiDeepBookSource::builder(SOURCE_ID)
        .with_rpc_endpoint(format!("http://{address}"))
        .with_deepbook_package_id("0xdeepbook")
        .with_poll_interval_ms(150)
        .with_request_limit(25)
        .with_start_from_beginning()
        .build()
        .context("Failed to build Sui DeepBook source")?;

    let query = Query::cypher(QUERY_ID)
        .query(
            r#"
            MATCH (e:DeepBookEvent)
            RETURN e.entity_id AS entity_id, e.change_type AS change_type, e.event_type AS event_type
        "#,
        )
        .from_source(SOURCE_ID)
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReaction::builder("app-reaction")
        .with_query(QUERY_ID)
        .build();

    let drasi = DrasiLib::builder()
        .with_id("sui-deepbook-integration")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .context("Failed to build DrasiLib")?;

    drasi.start().await.context("Failed to start DrasiLib")?;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(2)))
        .await
        .context("Failed to subscribe to application reaction output")?;

    let mut seen_ids: Vec<String> = Vec::new();

    wait_for_change(&mut subscription, 20, &mut seen_ids, |entry| {
        diff_matches(entry, "ADD", "order:42")
    })
    .await
    .context("Did not observe INSERT/ADD change")?;

    wait_for_change(&mut subscription, 20, &mut seen_ids, |entry| {
        update_matches(entry, "order:42")
    })
    .await
    .context("Did not observe UPDATE change")?;

    wait_for_change(&mut subscription, 20, &mut seen_ids, |entry| {
        diff_matches(entry, "DELETE", "order:42")
    })
    .await
    .context("Did not observe DELETE change")?;

    // Verify that events from "0xother-pkg" were filtered out —
    // no entity_id should reference the non-DeepBook package events.
    let leaked: Vec<&String> = seen_ids
        .iter()
        .filter(|id| !id.starts_with("order:42") && !id.starts_with("pool:"))
        .collect();
    assert!(
        leaked.is_empty(),
        "Package-ID filtering is broken: non-DeepBook entity_ids leaked through: {leaked:?}"
    );

    drasi.stop().await.context("Failed to stop DrasiLib")?;
    server_handle.abort();
    Ok(())
}
