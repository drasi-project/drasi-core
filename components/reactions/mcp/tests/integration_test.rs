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

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use mcp_core::types::{
    ClientCapabilities, Implementation, InitializeRequest, LATEST_PROTOCOL_VERSION,
};
use reqwest::header::AUTHORIZATION;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use drasi_lib::{DrasiLib, Query};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};

use drasi_reaction_mcp::{McpReaction, NotificationTemplate, QueryConfig};

struct McpTestClient {
    base_url: String,
    session_id: String,
    token: String,
    client: reqwest::Client,
    rx: mpsc::UnboundedReceiver<Value>,
}

impl McpTestClient {
    async fn connect(base_url: String, token: &str) -> Result<Self> {
        let client = reqwest::Client::new();
        let init_request = InitializeRequest {
            protocol_version: LATEST_PROTOCOL_VERSION.as_str().to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "mcp-test-client".to_string(),
                version: "0.1.0".to_string(),
            },
        };

        let response = client
            .post(&base_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": init_request,
            }))
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            return Err(anyhow::anyhow!("Initialize failed: {}", response.status()));
        }

        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("Missing mcp-session-id header"))?
            .to_string();

        let sse_response = client
            .get(&base_url)
            .header("mcp-session-id", &session_id)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await?;

        if sse_response.status() != StatusCode::OK {
            return Err(anyhow::anyhow!(
                "SSE connection failed: {}",
                sse_response.status()
            ));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut stream = sse_response.bytes_stream().eventsource();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => {
                        if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
                            let _ = tx.send(value);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            base_url,
            session_id,
            token: token.to_string(),
            client,
            rx,
        })
    }

    async fn request(&self, id: u64, method: &str, params: Value) -> Result<Value> {
        let response = self
            .client
            .post(&self.base_url)
            .header("mcp-session-id", &self.session_id)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .send()
            .await?;

        let body: Value = response.json().await?;
        Ok(body["result"].clone())
    }

    async fn subscribe(&self, uri: &str) -> Result<()> {
        self.request(2, "resources/subscribe", json!({ "uri": uri }))
            .await?;
        Ok(())
    }

    async fn next_notification(&mut self, timeout: Duration) -> Result<Value> {
        let value = tokio::time::timeout(timeout, self.rx.recv()).await?;
        value.ok_or_else(|| anyhow::anyhow!("Notification channel closed"))
    }
}

async fn wait_for_port(handle: &Arc<AtomicU16>) -> Result<u16> {
    for _ in 0..50 {
        let port = handle.load(Ordering::SeqCst);
        if port != 0 {
            return Ok(port);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow::anyhow!("Timed out waiting for server port"))
}

#[tokio::test]
#[ignore]
async fn test_mcp_reaction_end_to_end() -> Result<()> {
    let (source, source_handle) = ApplicationSource::new(
        "test-source",
        ApplicationSourceConfig {
            properties: std::collections::HashMap::new(),
        },
    )?;

    let query = Query::cypher("test-query")
        .query("MATCH (n:TestNode) RETURN n.id AS id, n.value AS value")
        .from_source("test-source")
        .build();

    let reaction = McpReaction::builder("test-reaction")
        .with_query("test-query")
        .with_port(0)
        .with_bearer_token("test-secret-token")
        .with_route(
            "test-query",
            QueryConfig {
                title: Some("Test Query".to_string()),
                description: Some("Test query resource".to_string()),
                added: Some(NotificationTemplate {
                    template: r#"{"op":"add","id":{{after.id}},"value":"{{after.value}}"}"#
                        .to_string(),
                }),
                updated: Some(NotificationTemplate {
                    template: r#"{"op":"update","id":{{after.id}},"old":"{{before.value}}","new":"{{after.value}}"}"#
                        .to_string(),
                }),
                deleted: Some(NotificationTemplate {
                    template: r#"{"op":"delete","id":{{before.id}}}"#.to_string(),
                }),
            },
        )
        .build()?;

    let port_handle = reaction.bound_port_handle();

    let drasi = DrasiLib::builder()
        .with_id("test-core")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;

    let port = wait_for_port(&port_handle).await?;
    let base_url = format!("http://127.0.0.1:{port}/");

    let init_request = InitializeRequest {
        protocol_version: LATEST_PROTOCOL_VERSION.as_str().to_string(),
        capabilities: ClientCapabilities::default(),
        client_info: Implementation {
            name: "unauth-client".to_string(),
            version: "0.1.0".to_string(),
        },
    };

    let unauthorized = reqwest::Client::new()
        .post(&base_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": init_request,
        }))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let mut client = McpTestClient::connect(base_url.clone(), "test-secret-token").await?;
    client.subscribe("drasi://query/test-query").await?;

    let insert_props = PropertyMapBuilder::new()
        .with_integer("id", 1)
        .with_string("value", "foo")
        .build();
    source_handle
        .send_node_insert("node-1", vec!["TestNode"], insert_props)
        .await?;

    let notification = client.next_notification(Duration::from_secs(5)).await?;
    assert_eq!(notification["params"]["operation"], "added");
    assert_eq!(notification["params"]["data"]["op"], "add");
    assert_eq!(notification["params"]["data"]["id"], 1);

    let update_props = PropertyMapBuilder::new()
        .with_integer("id", 1)
        .with_string("value", "bar")
        .build();
    source_handle
        .send_node_update("node-1", vec!["TestNode"], update_props)
        .await?;

    let notification = client.next_notification(Duration::from_secs(5)).await?;
    assert_eq!(notification["params"]["operation"], "updated");
    assert_eq!(notification["params"]["data"]["op"], "update");
    assert_eq!(notification["params"]["data"]["new"], "bar");

    source_handle
        .send_delete("node-1", vec!["TestNode"])
        .await?;
    let notification = client.next_notification(Duration::from_secs(5)).await?;
    assert_eq!(notification["params"]["operation"], "deleted");
    assert_eq!(notification["params"]["data"]["op"], "delete");

    let list_result = client.request(10, "resources/list", json!({})).await?;
    let resources = list_result["resources"].as_array().unwrap();
    assert!(resources
        .iter()
        .any(|resource| { resource["uri"] == "drasi://query/test-query" }));

    let insert_props = PropertyMapBuilder::new()
        .with_integer("id", 2)
        .with_string("value", "baz")
        .build();
    source_handle
        .send_node_insert("node-2", vec!["TestNode"], insert_props)
        .await?;
    let _ = client.next_notification(Duration::from_secs(5)).await?;

    let read_result = client
        .request(
            11,
            "resources/read",
            json!({ "uri": "drasi://query/test-query" }),
        )
        .await?;
    let text = read_result["contents"][0]["text"]
        .as_str()
        .expect("text contents");
    let items: Vec<Value> = serde_json::from_str(text)?;
    assert!(items.iter().any(|item| item["id"] == 2));

    drasi.stop().await?;

    Ok(())
}
