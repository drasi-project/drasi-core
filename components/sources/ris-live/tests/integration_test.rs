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

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::{Subscription, SubscriptionOptions};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_ris_live::RisLiveSource;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

enum HarnessAction {
    Announce { next_hop: String, msg_id: String },
    Withdraw { msg_id: String },
    PeerState { state: String, msg_id: String },
}

async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    tokio::time::sleep(Duration::from_millis(50)).await;
    port
}

fn routes_contains_value(value: &serde_json::Value, next_hop: &str) -> bool {
    value["prefix"] == "203.0.113.0/24" && value["next_hop"] == next_hop
}

async fn wait_for_result<F>(
    subscription: &mut Subscription,
    timeout: Duration,
    mut predicate: F,
) -> bool
where
    F: FnMut(&drasi_lib::channels::QueryResult) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(result) = subscription.recv().await {
            if predicate(&result) {
                return true;
            }
        }
    }
    false
}

async fn start_mock_ris_server(port: u16) -> mpsc::Sender<HarnessAction> {
    let (tx, mut rx) = mpsc::channel::<HarnessAction>(32);

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind mock server");
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws_stream = accept_async(stream).await.expect("websocket accept");

        // Receive ris_subscribe from source.
        let subscribe_frame = ws_stream
            .next()
            .await
            .expect("subscribe frame")
            .expect("frame");
        match subscribe_frame {
            Message::Text(text) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&text).expect("json subscribe");
                assert_eq!(parsed["type"], "ris_subscribe");
            }
            other => panic!("expected text subscribe frame, got {other:?}"),
        }

        // Send subscribe acknowledgement.
        ws_stream
            .send(Message::Text(
                json!({
                    "type": "ris_subscribe_ok",
                    "data": {
                        "subscription": { "host": "rrc00", "type": "UPDATE" },
                        "socketOptions": { "acknowledge": true, "includeRaw": false }
                    }
                })
                .to_string(),
            ))
            .await
            .expect("send subscribe ok");

        while let Some(action) = rx.recv().await {
            let payload = match action {
                HarnessAction::Announce { next_hop, msg_id } => json!({
                    "type": "ris_message",
                    "data": {
                        "timestamp": 1773098494.83,
                        "peer": "208.80.153.193",
                        "peer_asn": "14907",
                        "id": msg_id,
                        "host": "rrc00.ripe.net",
                        "type": "UPDATE",
                        "path": [14907, 3356, 64496],
                        "origin": "IGP",
                        "community": [[3356, 5]],
                        "announcements": [
                            { "next_hop": next_hop, "prefixes": ["203.0.113.0/24"] }
                        ],
                        "withdrawals": []
                    }
                }),
                HarnessAction::Withdraw { msg_id } => json!({
                    "type": "ris_message",
                    "data": {
                        "timestamp": 1773098495.83,
                        "peer": "208.80.153.193",
                        "peer_asn": "14907",
                        "id": msg_id,
                        "host": "rrc00.ripe.net",
                        "type": "UPDATE",
                        "withdrawals": ["203.0.113.0/24"]
                    }
                }),
                HarnessAction::PeerState { state, msg_id } => json!({
                    "type": "ris_message",
                    "data": {
                        "timestamp": 1773098496.83,
                        "peer": "208.80.153.193",
                        "peer_asn": "14907",
                        "id": msg_id,
                        "host": "rrc00.ripe.net",
                        "type": "RIS_PEER_STATE",
                        "state": state
                    }
                }),
            };

            ws_stream
                .send(Message::Text(payload.to_string()))
                .await
                .expect("send payload");
        }

        let _ = ws_stream.close(None).await;
    });

    tx
}

#[tokio::test]
#[ignore]
async fn test_change_detection_with_websocket_harness() {
    let port = find_available_port().await;
    let harness_tx = start_mock_ris_server(port).await;

    let source = RisLiveSource::builder("ris-source")
        .with_websocket_url(format!("ws://127.0.0.1:{port}/v1/ws/"))
        .with_host("rrc00")
        .with_start_from_now()
        .with_auto_start(true)
        .build()
        .unwrap();

    let routes_query = Query::cypher("routes-query")
        .query(
            "MATCH (peer:Peer)-[r:ROUTES]->(p:Prefix) \
             RETURN peer.peer_ip AS peer, p.prefix AS prefix, r.next_hop AS next_hop",
        )
        .from_source("ris-source")
        .auto_start(true)
        .build();

    let peer_query = Query::cypher("peer-query")
        .query("MATCH (peer:Peer) RETURN peer.peer_ip AS peer, peer.state AS state")
        .from_source("ris-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("app-reaction")
        .with_queries(vec!["routes-query".to_string(), "peer-query".to_string()])
        .build();

    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("ris-live-integration-test")
            .with_source(source)
            .with_query(routes_query)
            .with_query(peer_query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    drasi.start().await.unwrap();

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(1)))
        .await
        .unwrap();

    // CREATE/INSERT: announcement creates ROUTES relationship
    harness_tx
        .send(HarnessAction::Announce {
            next_hop: "198.51.100.1".to_string(),
            msg_id: "msg-insert".to_string(),
        })
        .await
        .unwrap();

    let found_insert = wait_for_result(&mut subscription, Duration::from_secs(10), |result| {
        if result.query_id != "routes-query" {
            return false;
        }
        result.results.iter().any(|diff| match diff {
            ResultDiff::Add { data } => routes_contains_value(data, "198.51.100.1"),
            _ => false,
        })
    })
    .await;
    assert!(found_insert, "INSERT announcement was not detected");

    // UPDATE: re-announcement updates next_hop
    harness_tx
        .send(HarnessAction::Announce {
            next_hop: "198.51.100.2".to_string(),
            msg_id: "msg-update".to_string(),
        })
        .await
        .unwrap();

    let found_update = wait_for_result(&mut subscription, Duration::from_secs(10), |result| {
        if result.query_id != "routes-query" {
            return false;
        }
        result.results.iter().any(|diff| match diff {
            ResultDiff::Update { after, .. } => routes_contains_value(after, "198.51.100.2"),
            _ => false,
        })
    })
    .await;
    assert!(found_update, "UPDATE announcement was not detected");

    // DELETE: withdrawal removes ROUTES relationship
    harness_tx
        .send(HarnessAction::Withdraw {
            msg_id: "msg-delete".to_string(),
        })
        .await
        .unwrap();

    let found_delete = wait_for_result(&mut subscription, Duration::from_secs(10), |result| {
        if result.query_id != "routes-query" {
            return false;
        }
        result.results.iter().any(|diff| match diff {
            ResultDiff::Delete { data } => data["prefix"] == "203.0.113.0/24",
            _ => false,
        })
    })
    .await;
    assert!(found_delete, "DELETE withdrawal was not detected");

    // Peer state update
    harness_tx
        .send(HarnessAction::PeerState {
            state: "down".to_string(),
            msg_id: "msg-peer-state".to_string(),
        })
        .await
        .unwrap();

    let found_peer_state = wait_for_result(&mut subscription, Duration::from_secs(10), |result| {
        if result.query_id != "peer-query" {
            return false;
        }
        result.results.iter().any(|diff| match diff {
            ResultDiff::Update { after, .. } => after["state"] == "down",
            ResultDiff::Add { data } => data["state"] == "down",
            _ => false,
        })
    })
    .await;
    assert!(found_peer_state, "RIS_PEER_STATE update was not detected");

    drasi.stop().await.unwrap();
}
