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

//! Protocol-target integration test for dashboard reaction.

mod mock_source;

use anyhow::{Context, Result};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_dashboard::{DashboardConfig, DashboardReaction};
use futures::stream::Stream;
use futures::{SinkExt, StreamExt};
use mock_source::{MockSource, PropertyMapBuilder};
use serde_json::Value;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

async fn next_query_result<S>(stream: &mut S) -> Result<Value>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let maybe_message = timeout(Duration::from_secs(10), stream.next())
            .await
            .context("timed out waiting for websocket message")?;
        let Some(message_result) = maybe_message else {
            return Err(anyhow::anyhow!("websocket stream ended unexpectedly"));
        };

        let message = message_result.context("websocket receive failed")?;
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(text.as_ref())
                .context("websocket payload was not valid json")?;
            if value.get("type").and_then(Value::as_str) == Some("query_result") {
                return Ok(value);
            }
        }
    }
}

#[tokio::test]
async fn test_dashboard_reaction_end_to_end() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let (mock_source, source_handle) = MockSource::new("test-source")?;

    let query = Query::cypher("test-query")
        .query(
            r#"
            MATCH (p:Person)
            RETURN p.name AS name, p.age AS age
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    let reaction = DashboardReaction::builder("test-dashboard")
        .with_query("test-query")
        .with_port(19110)
        .build()?;

    let core = DrasiLib::builder()
        .with_id("dashboard-test-core")
        .with_source(mock_source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;

    // Poll until the server is ready (max 10 seconds).
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(resp) = client.get("http://localhost:19110/").send().await {
            if resp.status().is_success() {
                break;
            }
        }
        if tokio::time::Instant::now() > deadline {
            return Err(anyhow::anyhow!(
                "server did not become ready within 10 seconds"
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }

    // Verify static UI endpoint.
    let root_response = client.get("http://localhost:19110/").send().await?;
    assert_eq!(root_response.status(), reqwest::StatusCode::OK);
    let root_body = root_response.text().await?;
    assert!(
        root_body.contains("Drasi Dashboard"),
        "dashboard index page should be served"
    );

    // Dashboard CRUD via REST.
    let create_response = client
        .post("http://localhost:19110/api/dashboards")
        .json(&serde_json::json!({
            "name": "Integration Dashboard",
            "gridOptions": {"columns": 12, "rowHeight": 60, "margin": 10},
            "widgets": []
        }))
        .send()
        .await?;
    assert_eq!(create_response.status(), reqwest::StatusCode::CREATED);
    let created_dashboard: DashboardConfig = create_response.json().await?;

    let get_response = client
        .get(format!(
            "http://localhost:19110/api/dashboards/{}",
            created_dashboard.id
        ))
        .send()
        .await?;
    assert_eq!(get_response.status(), reqwest::StatusCode::OK);

    let update_response = client
        .put(format!(
            "http://localhost:19110/api/dashboards/{}",
            created_dashboard.id
        ))
        .json(&serde_json::json!({"name": "Integration Dashboard Updated"}))
        .send()
        .await?;
    assert_eq!(update_response.status(), reqwest::StatusCode::OK);

    let delete_response = client
        .delete(format!(
            "http://localhost:19110/api/dashboards/{}",
            created_dashboard.id
        ))
        .send()
        .await?;
    assert_eq!(delete_response.status(), reqwest::StatusCode::NO_CONTENT);

    // WebSocket protocol harness.
    let (ws_stream, _) = connect_async("ws://localhost:19110/ws").await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    ws_sender
        .send(Message::Text(
            r#"{"type":"subscribe","query_ids":["test-query"]}"#.to_string(),
        ))
        .await?;
    // Brief yield to let server process the subscribe before we send data.
    tokio::task::yield_now().await;
    sleep(Duration::from_millis(50)).await;

    // INSERT verification.
    source_handle
        .send_node_insert(
            "person-1",
            vec!["Person"],
            PropertyMapBuilder::new()
                .with_string("name", "Alice")
                .with_integer("age", 30)
                .build(),
        )
        .await?;
    let insert_message = next_query_result(&mut ws_receiver).await?;
    assert!(
        insert_message["results"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|result| result["op"] == "add"),
        "INSERT operation should produce add diff message"
    );

    // UPDATE verification.
    source_handle
        .send_node_update(
            "person-1",
            vec!["Person"],
            PropertyMapBuilder::new()
                .with_string("name", "Alice Updated")
                .with_integer("age", 31)
                .build(),
        )
        .await?;
    let update_message = next_query_result(&mut ws_receiver).await?;
    assert!(
        update_message["results"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|result| {
                result["op"] == "update"
                    && result
                        .get("after")
                        .and_then(|after| after.get("name"))
                        .and_then(Value::as_str)
                        == Some("Alice Updated")
            }),
        "UPDATE operation should produce update diff message with updated payload"
    );

    // DELETE verification.
    source_handle
        .send_node_delete("person-1", vec!["Person"])
        .await?;
    let delete_message = next_query_result(&mut ws_receiver).await?;
    assert!(
        delete_message["results"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|result| result["op"] == "delete"),
        "DELETE operation should produce delete diff message"
    );

    let _ = ws_sender.send(Message::Close(None)).await;
    core.stop().await?;

    Ok(())
}

/// Regression test for the "stale tab after reaction restart" bug: stopping the
/// dashboard reaction must gracefully shut the HTTP/WebSocket server down —
/// closing active WebSocket connections and releasing the listener — so a client
/// can't keep talking to (or reuse a pooled connection against) the old server.
#[tokio::test]
async fn test_dashboard_server_shuts_down_on_stop() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let (mock_source, _source_handle) = MockSource::new("shutdown-source")?;

    let query = Query::cypher("shutdown-query")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("shutdown-source")
        .auto_start(true)
        .build();

    let reaction = DashboardReaction::builder("shutdown-dashboard")
        .with_query("shutdown-query")
        .with_port(19111)
        .build()?;

    let core = DrasiLib::builder()
        .with_id("dashboard-shutdown-core")
        .with_source(mock_source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;

    // Wait for the server to come up.
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(resp) = client.get("http://localhost:19111/").send().await {
            if resp.status().is_success() {
                break;
            }
        }
        if tokio::time::Instant::now() > deadline {
            return Err(anyhow::anyhow!("server did not become ready"));
        }
        sleep(Duration::from_millis(50)).await;
    }

    // Verify no-cache headers are set on served assets.
    let root_response = client.get("http://localhost:19111/").send().await?;
    let cache_control = root_response
        .headers()
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        cache_control.contains("no-cache"),
        "served assets should carry a no-cache Cache-Control header, got: {cache_control:?}"
    );

    // Open a live WebSocket so we can observe it being closed on shutdown.
    let (ws_stream, _) = connect_async("ws://localhost:19111/ws").await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    ws_sender
        .send(Message::Text(
            r#"{"type":"subscribe","query_ids":["shutdown-query"]}"#.to_string(),
        ))
        .await?;
    sleep(Duration::from_millis(50)).await;

    // Stop the reaction (and the rest of the core).
    core.stop().await?;

    // The active WebSocket must be closed by the graceful shutdown: the stream
    // should end (None) or error within a short window, rather than hanging open.
    let ws_closed = timeout(Duration::from_secs(5), async {
        loop {
            match ws_receiver.next().await {
                None => break true,
                Some(Ok(Message::Close(_))) => break true,
                Some(Err(_)) => break true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        ws_closed,
        "active WebSocket should be closed when the reaction stops"
    );

    // The HTTP listener must be released: new requests to the old port should
    // fail (connection refused) once shutdown completes.
    let released = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match client.get("http://localhost:19111/").send().await {
                Err(_) => break true,
                Ok(_) => {
                    if tokio::time::Instant::now() > deadline {
                        break false;
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            }
        }
    };
    assert!(
        released,
        "HTTP server should stop accepting connections after the reaction stops"
    );

    Ok(())
}
