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

use std::{collections::HashSet, time::Duration};

use drasi_core::models::{Element, SourceChange};
use drasi_lib::{
    channels::SourceEvent, component_graph::ComponentUpdate, ComponentStatus, Source,
    SourceRuntimeContext, SourceSubscriptionSettings,
};
use drasi_source_websocket::{
    EffectiveFromConfig, ElementTemplate, ElementType, HeaderConfig, MappingCondition,
    OperationType, ReconnectConfig, SourceMapping, TimestampFormat, WebSocketSource,
    WebSocketSourceConfig,
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::TcpListener,
    sync::oneshot,
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    accept_async, accept_hdr_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::{header::AUTHORIZATION, StatusCode},
        protocol::{frame::coding::CloseCode, CloseFrame},
        Message,
    },
};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_INITIAL_MESSAGE: &str = r#"{"type":"authenticate"}"#;
const SECOND_INITIAL_MESSAGE: &str = r#"{"type":"subscribe","stream":"sensors"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maps_messages_and_resends_initial_messages_after_abrupt_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut received_initial_messages = Vec::new();

        for connection_index in 0..2 {
            let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut socket = accept_hdr_async(stream, validate_handshake).await.unwrap();

            let mut initial_messages = Vec::new();
            for _ in 0..2 {
                match timeout(STEP_TIMEOUT, socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                {
                    Message::Text(text) => initial_messages.push(text),
                    other => panic!("expected initial text message, got {other:?}"),
                }
            }
            received_initial_messages.push(initial_messages);

            let payload = if connection_index == 0 {
                r#"{"op":"insert","id":"sensor-1","value":10,"ts":1000}"#
            } else {
                r#"{"op":"update","id":"sensor-1","value":20,"ts":1001}"#
            };
            socket
                .send(Message::Text(payload.to_string()))
                .await
                .unwrap();

            if connection_index == 0 {
                drop(socket);
            } else {
                loop {
                    match timeout(STEP_TIMEOUT, socket.next()).await {
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                        Ok(Some(Ok(Message::Ping(payload)))) => {
                            socket.send(Message::Pong(payload)).await.unwrap();
                        }
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(error))) => panic!("server read failed: {error}"),
                        Err(_) => panic!("server timed out waiting for client close"),
                    }
                }
            }
        }

        received_initial_messages
    });

    let config = WebSocketSourceConfig {
        url: format!("ws://{address}/events?tenant=test"),
        allow_insecure: true,
        headers: vec![HeaderConfig {
            name: "Authorization".to_string(),
            value: "Bearer test-token".to_string(),
        }],
        connect_timeout_ms: 2_000,
        initial_messages: vec![
            FIRST_INITIAL_MESSAGE.to_string(),
            SECOND_INITIAL_MESSAGE.to_string(),
        ],
        reconnect: ReconnectConfig {
            enabled: true,
            delay_ms: 100,
            max_delay_ms: None,
        },
        mappings: vec![sensor_mapping()],
        buffer_capacity: 8,
        ..Default::default()
    };
    let source = WebSocketSource::new("websocket-test", config).unwrap();
    let response = source.subscribe(subscription_settings()).await.unwrap();
    let mut receiver = response.receiver;

    source.start().await.unwrap();
    wait_for_status(&source, ComponentStatus::Running).await;
    assert_eq!(source.status().await, ComponentStatus::Running);
    assert!(!source.supports_replay());

    let insert = timeout(STEP_TIMEOUT, receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(insert.source_position.is_none());
    assert_change(&insert.event, "sensor-1", false);

    let update = timeout(STEP_TIMEOUT, receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(update.source_position.is_none());
    assert_change(&update.event, "sensor-1", true);

    source.stop().await.unwrap();
    assert_eq!(source.status().await, ComponentStatus::Stopped);

    let received_initial_messages = timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
    let expected = vec![
        FIRST_INITIAL_MESSAGE.to_string(),
        SECOND_INITIAL_MESSAGE.to_string(),
    ];
    assert_eq!(received_initial_messages, vec![expected.clone(), expected]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_for_a_subscriber_before_connecting_and_answers_ping() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let source = WebSocketSource::new(
        "websocket-ping-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();

    timeout(Duration::from_millis(200), source.start())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.status().await, ComponentStatus::Starting);
    assert!(timeout(Duration::from_millis(200), listener.accept())
        .await
        .is_err());

    let (pong_tx, pong_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let ping_payload = vec![1, 2, 3];
        socket
            .send(Message::Ping(ping_payload.clone()))
            .await
            .unwrap();

        loop {
            match timeout(STEP_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(Message::Pong(payload)))) => {
                    assert_eq!(payload, ping_payload);
                    let _ = pong_tx.send(());
                    break;
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                    panic!("client closed before answering Ping")
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("server read failed: {error}"),
                Err(_) => panic!("server timed out waiting for Pong"),
            }
        }

        loop {
            match timeout(STEP_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("server read failed: {error}"),
                Err(_) => panic!("server timed out waiting for client close"),
            }
        }
    });

    let _response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-ping-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    timeout(STEP_TIMEOUT, pong_rx).await.unwrap().unwrap();
    wait_for_status(&source, ComponentStatus::Running).await;

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_cancels_an_initial_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = accepted_tx.send(());
        let _ = release_rx.await;
        drop(stream);
    });

    let source = WebSocketSource::new(
        "websocket-cancel-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            connect_timeout_ms: 5_000,
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();
    let _response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-cancel-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    source.start().await.unwrap();

    timeout(STEP_TIMEOUT, accepted_rx).await.unwrap().unwrap();
    timeout(STEP_TIMEOUT, source.stop()).await.unwrap().unwrap();
    assert_eq!(source.status().await, ComponentStatus::Stopped);

    let _ = release_tx.send(());
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maps_valid_array_item_after_ignored_ack_and_heartbeat() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(r#"{"type":"ack"}"#.to_string()))
            .await
            .unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"batch","events":[{"type":"heartbeat"}]}"#.to_string(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"batch","events":[{"op":"insert","id":"sensor-1","value":10,"ts":1000}]}"#.to_string(),
            ))
            .await
            .unwrap();

        loop {
            match timeout(STEP_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("server read failed: {error}"),
                Err(_) => panic!("server timed out waiting for client close"),
            }
        }
    });

    let mut mapping = sensor_mapping();
    mapping.when = Some(MappingCondition {
        header: None,
        field: Some("envelope.type".to_string()),
        equals: Some("batch".to_string()),
        contains: None,
        regex: None,
    });
    let source = WebSocketSource::new(
        "websocket-operation-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            items_path: "events".to_string(),
            mappings: vec![mapping],
            ..Default::default()
        },
    )
    .unwrap();
    let response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-operation-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    let mut receiver = response.receiver;

    source.start().await.unwrap();
    let event = timeout(STEP_TIMEOUT, receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_change(&event.event, "sensor-1", false);
    assert_eq!(source.status().await, ComponentStatus::Running);

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_http_429_then_connects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(accept_hdr_async(stream, reject_too_many_requests)
            .await
            .is_err());

        let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                r#"{"op":"insert","id":"sensor-429","value":10,"ts":1000}"#.to_string(),
            ))
            .await
            .unwrap();

        loop {
            match timeout(STEP_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("server read failed: {error}"),
                Err(_) => panic!("server timed out waiting for client close"),
            }
        }
    });

    let source = WebSocketSource::new(
        "websocket-rate-limit-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/path-secret?token=query-secret"),
            allow_insecure: true,
            headers: vec![HeaderConfig {
                name: "Authorization".to_string(),
                value: "header-secret".to_string(),
            }],
            initial_messages: vec![r#"{"token":"message-secret"}"#.to_string()],
            reconnect: ReconnectConfig {
                enabled: true,
                delay_ms: 100,
                max_delay_ms: None,
            },
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();
    let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(16);
    source
        .initialize(SourceRuntimeContext::new(
            "websocket-test-instance",
            "websocket-rate-limit-test",
            None,
            update_tx,
            None,
        ))
        .await;
    let response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-rate-limit-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    let mut receiver = response.receiver;

    source.start().await.unwrap();
    let event = timeout(STEP_TIMEOUT, receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_change(&event.event, "sensor-429", false);
    assert_eq!(source.status().await, ComponentStatus::Running);

    let mut saw_retry_status = false;
    while let Ok(ComponentUpdate::Status {
        status, message, ..
    }) = update_rx.try_recv()
    {
        if let Some(message) = message {
            saw_retry_status |= status == ComponentStatus::Starting && message.contains("retrying");
            for secret in [
                "path-secret",
                "query-secret",
                "header-secret",
                "message-secret",
            ] {
                assert!(!message.contains(secret));
            }
        }
    }
    assert!(saw_retry_status);

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_an_initial_refused_connection_then_stops_on_401() {
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);

    let source = WebSocketSource::new(
        "websocket-initial-retry-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            reconnect: ReconnectConfig {
                enabled: true,
                delay_ms: 100,
                max_delay_ms: None,
            },
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();
    let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(16);
    source
        .initialize(SourceRuntimeContext::new(
            "websocket-test-instance",
            "websocket-initial-retry-test",
            None,
            update_tx,
            None,
        ))
        .await;
    let _response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-initial-retry-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();

    source.start().await.unwrap();
    timeout(STEP_TIMEOUT, async {
        while let Some(ComponentUpdate::Status {
            status, message, ..
        }) = update_rx.recv().await
        {
            if status == ComponentStatus::Starting
                && message.is_some_and(|message| message.contains("retrying"))
            {
                return;
            }
        }
        panic!("source stopped reporting status before retrying");
    })
    .await
    .expect("source did not report the refused connection as retryable");
    assert_eq!(source.status().await, ComponentStatus::Starting);

    let listener = TcpListener::bind(address).await.unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(accept_hdr_async(stream, reject_unauthorized).await.is_err());
        assert!(timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_err());
    });

    wait_for_status(&source, ComponentStatus::Error).await;

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_after_fatal_auth_error_during_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "reconnect".into(),
            })))
            .await
            .unwrap();

        let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(accept_hdr_async(stream, reject_unauthorized).await.is_err());
        assert!(timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_err());
    });

    let source = WebSocketSource::new(
        "websocket-rejection-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            reconnect: ReconnectConfig {
                enabled: true,
                delay_ms: 100,
                max_delay_ms: None,
            },
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();
    let _response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-rejection-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();

    source.start().await.unwrap();
    wait_for_status(&source, ComponentStatus::Error).await;

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_messages_are_fatal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let payload = serde_json::json!({
            "op": "insert",
            "id": "oversized",
            "value": "x".repeat(2_048),
            "ts": 1_000,
        })
        .to_string();
        socket.send(Message::Text(payload)).await.unwrap();
        let _ = timeout(STEP_TIMEOUT, socket.next()).await;
    });

    let source = WebSocketSource::new(
        "websocket-oversized-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            mappings: vec![sensor_mapping()],
            max_message_size_bytes: 1_024,
            ..Default::default()
        },
    )
    .unwrap();
    let response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-oversized-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    let mut receiver = response.receiver;

    source.start().await.unwrap();
    assert!(timeout(STEP_TIMEOUT, receiver.recv())
        .await
        .unwrap()
        .is_err());
    wait_for_status(&source, ComponentStatus::Error).await;
    assert_eq!(source.status().await, ComponentStatus::Error);

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_is_skipped_before_a_valid_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(Message::Text("{invalid".to_string()))
            .await
            .unwrap();
        socket
            .send(Message::Text(
                r#"{"op":"insert","id":"after-malformed","value":10,"ts":1000}"#.to_string(),
            ))
            .await
            .unwrap();

        loop {
            match timeout(STEP_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("server read failed: {error}"),
                Err(_) => panic!("server timed out waiting for client close"),
            }
        }
    });

    let source = WebSocketSource::new(
        "websocket-malformed-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();
    let response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-malformed-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    let mut receiver = response.receiver;

    source.start().await.unwrap();
    let event = timeout(STEP_TIMEOUT, receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_change(&event.event, "after-malformed", false);
    assert_eq!(source.status().await, ComponentStatus::Running);

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_waits_for_a_fresh_subscription() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for id in ["before-restart", "after-restart"] {
            let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "op": "insert",
                        "id": id,
                        "value": 10,
                        "ts": 1_000,
                    })
                    .to_string(),
                ))
                .await
                .unwrap();

            loop {
                match timeout(STEP_TIMEOUT, socket.next()).await {
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(error))) => panic!("server read failed: {error}"),
                    Err(_) => panic!("server timed out waiting for client close"),
                }
            }
        }
    });

    let source = WebSocketSource::new(
        "websocket-restart-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();

    let first = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-restart-test".to_string(),
            query_id: "first-query".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    let mut first_receiver = first.receiver;
    source.start().await.unwrap();
    let event = timeout(STEP_TIMEOUT, first_receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_change(&event.event, "before-restart", false);
    source.stop().await.unwrap();
    drop(first_receiver);

    source.start().await.unwrap();
    assert_eq!(source.status().await, ComponentStatus::Starting);
    let second = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-restart-test".to_string(),
            query_id: "second-query".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();
    let mut second_receiver = second.receiver;
    let event = timeout(STEP_TIMEOUT, second_receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_change(&event.event, "after-restart", false);

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_messages_are_fatal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket.send(Message::Binary(vec![1, 2, 3])).await.unwrap();
        let _ = timeout(STEP_TIMEOUT, socket.next()).await;
    });

    let source = WebSocketSource::new(
        "websocket-binary-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();
    let _response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-binary-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();

    source.start().await.unwrap();
    wait_for_status(&source, ComponentStatus::Error).await;
    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn treats_http_404_as_fatal_without_retrying() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(STEP_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(accept_hdr_async(stream, reject_not_found).await.is_err());
        assert!(timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_err());
    });

    let source = WebSocketSource::new(
        "websocket-not-found-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/path-secret?token=query-secret"),
            allow_insecure: true,
            headers: vec![HeaderConfig {
                name: "Authorization".to_string(),
                value: "header-secret".to_string(),
            }],
            initial_messages: vec![r#"{"token":"message-secret"}"#.to_string()],
            reconnect: ReconnectConfig {
                enabled: true,
                delay_ms: 100,
                max_delay_ms: Some(400),
            },
            mappings: vec![sensor_mapping()],
            ..Default::default()
        },
    )
    .unwrap();
    let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(16);
    source
        .initialize(SourceRuntimeContext::new(
            "websocket-test-instance",
            "websocket-not-found-test",
            None,
            update_tx,
            None,
        ))
        .await;
    let _response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-not-found-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();

    source.start().await.unwrap();
    wait_for_status(&source, ComponentStatus::Error).await;

    let mut saw_error = false;
    while let Ok(ComponentUpdate::Status {
        status, message, ..
    }) = update_rx.try_recv()
    {
        saw_error |= status == ComponentStatus::Error;
        if let Some(message) = message {
            for secret in [
                "path-secret",
                "query-secret",
                "header-secret",
                "message-secret",
            ] {
                assert!(!message.contains(secret));
            }
        }
    }
    assert!(saw_error);

    source.stop().await.unwrap();
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_interrupts_dispatch_blocked_by_subscriber_backpressure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (blocked_tx, blocked_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        for id in ["buffered", "blocked"] {
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "op": "insert",
                        "id": id,
                        "value": 10,
                        "ts": 1_000,
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
        }
        socket.send(Message::Ping(vec![9])).await.unwrap();

        assert!(timeout(Duration::from_millis(300), socket.next())
            .await
            .is_err());
        let _ = blocked_tx.send(());

        let _ = timeout(STEP_TIMEOUT, socket.next()).await;
    });

    let source = WebSocketSource::new(
        "websocket-backpressure-stop-test",
        WebSocketSourceConfig {
            url: format!("ws://{address}/events"),
            allow_insecure: true,
            mappings: vec![sensor_mapping()],
            buffer_capacity: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let _response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "websocket-backpressure-stop-test".to_string(),
            ..subscription_settings()
        })
        .await
        .unwrap();

    source.start().await.unwrap();
    wait_for_status(&source, ComponentStatus::Running).await;
    timeout(STEP_TIMEOUT, blocked_rx).await.unwrap().unwrap();

    timeout(Duration::from_secs(1), source.stop())
        .await
        .expect("stop must not reach SourceBase's five-second abort fallback")
        .unwrap();
    assert_eq!(source.status().await, ComponentStatus::Stopped);
    timeout(STEP_TIMEOUT, server).await.unwrap().unwrap();
}

#[allow(clippy::result_large_err)]
fn validate_handshake(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
    assert_eq!(request.uri().path(), "/events");
    assert_eq!(request.uri().query(), Some("tenant=test"));
    assert_eq!(
        request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-token")
    );
    Ok(response)
}

#[allow(clippy::result_large_err)]
fn reject_unauthorized(_: &Request, _: Response) -> Result<Response, ErrorResponse> {
    Err(tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(Some("unauthorized".to_string()))
        .expect("unauthorized response should be valid"))
}

#[allow(clippy::result_large_err)]
fn reject_too_many_requests(_: &Request, _: Response) -> Result<Response, ErrorResponse> {
    Err(tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Some("rate limited".to_string()))
        .expect("rate-limit response should be valid"))
}

#[allow(clippy::result_large_err)]
fn reject_not_found(_: &Request, _: Response) -> Result<Response, ErrorResponse> {
    Err(tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Some("not found".to_string()))
        .expect("not-found response should be valid"))
}

fn sensor_mapping() -> SourceMapping {
    SourceMapping {
        when: None,
        operation: None,
        operation_from: Some("payload.op".to_string()),
        operation_map: Some(std::collections::HashMap::from([
            ("insert".to_string(), OperationType::Insert),
            ("update".to_string(), OperationType::Update),
        ])),
        element_type: ElementType::Node,
        effective_from: Some(EffectiveFromConfig::Explicit {
            value: "{{payload.ts}}".to_string(),
            format: TimestampFormat::UnixMillis,
        }),
        template: ElementTemplate {
            id: "{{payload.id}}".to_string(),
            labels: vec!["Sensor".to_string()],
            properties: Some(serde_json::json!({
                "value": "{{payload.value}}"
            })),
            from: None,
            to: None,
        },
    }
}

fn subscription_settings() -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: "websocket-test".to_string(),
        enable_bootstrap: false,
        query_id: "sensor-query".to_string(),
        nodes: HashSet::from(["Sensor".to_string()]),
        relations: HashSet::new(),
        resume_from: None,
        request_position_handle: false,
    }
}

fn assert_change(event: &SourceEvent, expected_id: &str, update: bool) {
    let element = match event {
        SourceEvent::Change(SourceChange::Insert { element }) if !update => element,
        SourceEvent::Change(SourceChange::Update { element }) if update => element,
        other => panic!("unexpected source event: {other:?}"),
    };
    match element {
        Element::Node { metadata, .. } => {
            assert_eq!(metadata.reference.element_id.as_ref(), expected_id);
            assert_eq!(metadata.labels[0].as_ref(), "Sensor");
        }
        other => panic!("expected node element, got {other:?}"),
    }
}

async fn wait_for_status(source: &WebSocketSource, expected: ComponentStatus) {
    timeout(STEP_TIMEOUT, async {
        loop {
            if source.status().await == expected {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("source did not reach the expected status");
}
