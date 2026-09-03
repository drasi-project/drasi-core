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

//! WAL (Write-Ahead Log) integration tests for the gRPC source.
//!
//! These tests verify durable event persistence, crash recovery, and replay
//! functionality using the redb WAL backend with the gRPC source.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::wal::{CapacityPolicy, WalProvider};
use drasi_lib::Source;
use drasi_source_grpc::proto::{
    source_service_client::SourceServiceClient, ChangeType, Element, ElementMetadata,
    ElementReference, Node, SourceChange as ProtoSourceChange, SubmitEventRequest,
};
use drasi_source_grpc::{GrpcSource, GrpcSourceConfig};
use drasi_wal_redb::RedbWalProvider;
use tempfile::TempDir;

/// Helper: build a `SubmitEventRequest` for an insert of a single node.
fn insert_node_request(source_id: &str, element_id: &str) -> SubmitEventRequest {
    use drasi_source_grpc::proto::element::Element as ProtoElementType;
    use drasi_source_grpc::proto::source_change::Change;

    SubmitEventRequest {
        event: Some(ProtoSourceChange {
            r#type: ChangeType::Insert as i32,
            change: Some(Change::Element(Element {
                element: Some(ProtoElementType::Node(Node {
                    metadata: Some(ElementMetadata {
                        reference: Some(ElementReference {
                            source_id: source_id.to_string(),
                            element_id: element_id.to_string(),
                        }),
                        labels: vec!["Person".to_string()],
                        effective_from: 0,
                    }),
                    properties: None,
                })),
            })),
            timestamp: None,
            source_id: source_id.to_string(),
        }),
    }
}

/// Reserve an ephemeral TCP port to avoid collisions under parallel test
/// execution (so the test can run in CI without a hard-coded port).
///
/// Binds `127.0.0.1:0`, reads the assigned port, and drops the listener so the
/// source can bind it. There is a tiny TOCTOU window, but this is far more
/// robust than a fixed port. Mirrors the dashboard reaction tests.
fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0") // DevSkim: ignore DS137138
        .expect("failed to reserve an ephemeral port");
    listener
        .local_addr()
        .expect("failed to read reserved port")
        .port()
}

/// Connect a gRPC client, retrying briefly while the server task finishes
/// binding the ephemeral port (avoids a startup race in CI).
async fn connect_with_retry(port: u16) -> SourceServiceClient<tonic::transport::Channel> {
    let url = format!("http://127.0.0.1:{port}");
    for _ in 0..50 {
        if let Ok(client) = SourceServiceClient::connect(url.clone()).await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("failed to connect gRPC client to {url} after retries");
}

/// Helper: create a DurabilityConfig with the given policy
fn durability_config(
    enabled: bool,
    max_events: u64,
    policy: CapacityPolicy,
) -> drasi_lib::DurabilityConfig {
    drasi_lib::DurabilityConfig {
        enabled,
        max_events,
        capacity_policy: policy,
    }
}

/// Helper: initialize a source with WAL provider
async fn init_source_with_wal(source: &dyn Source, wal: Arc<dyn WalProvider>, source_id: &str) {
    let (update_tx, _update_rx) = tokio::sync::mpsc::channel(16);
    source
        .initialize(SourceRuntimeContext {
            instance_id: "test-instance".to_string(),
            source_id: source_id.to_string(),
            update_tx,
            state_store: None,
            identity_provider: None,
            wal_provider: Some(wal),
        })
        .await;
}

// ============================================================
// gRPC Source WAL Tests
// ============================================================

#[tokio::test]
#[ignore]
async fn test_grpc_wal_disabled_no_persistence() {
    let config = GrpcSourceConfig {
        host: "127.0.0.1".to_string(),
        port: 19401,
        endpoint: None,
        timeout_ms: 5000,
        durability: None,
    };
    let source = GrpcSource::new("grpc-disabled", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "grpc-disabled").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(!source.supports_replay());

    let count = wal.event_count("grpc-disabled").await;
    assert!(count.is_err(), "WAL should not be registered when disabled");

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_grpc_wal_enabled_source_starts() {
    let config = GrpcSourceConfig {
        host: "127.0.0.1".to_string(),
        port: 19402,
        endpoint: None,
        timeout_ms: 5000,
        durability: Some(durability_config(
            true,
            10_000,
            CapacityPolicy::RejectIncoming,
        )),
    };
    let source = GrpcSource::new("grpc-enabled", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "grpc-enabled").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(source.supports_replay());

    let count = wal.event_count("grpc-enabled").await.unwrap();
    assert_eq!(count, 0);

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_grpc_crash_recovery_resumes_sequence() {
    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));

    // Manually write some events to WAL to simulate prior lifecycle
    let wal_config = drasi_lib::wal::WriteAheadLogConfig {
        max_events: 10_000,
        capacity_policy: CapacityPolicy::RejectIncoming,
    };
    wal.register("grpc-crash", wal_config).await.unwrap();

    // Simulate 5 prior events by appending directly
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    for i in 1..=5u64 {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference {
                        source_id: Arc::from("grpc-crash"),
                        element_id: Arc::from(format!("n{i}").as_str()),
                    },
                    labels: Arc::from(vec![Arc::from("T")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
            },
        };
        let seq = wal.append("grpc-crash", &change).await.unwrap();
        assert_eq!(seq, i);
    }

    // Now start a gRPC source — it should resume from head=5
    let config = GrpcSourceConfig {
        host: "127.0.0.1".to_string(),
        port: 19403,
        endpoint: None,
        timeout_ms: 5000,
        durability: Some(durability_config(
            true,
            10_000,
            CapacityPolicy::RejectIncoming,
        )),
    };
    let source = GrpcSource::new("grpc-crash", config).unwrap();
    init_source_with_wal(&source, wal.clone(), "grpc-crash").await;
    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Subscribe with resume to verify replay works
    let settings = SourceSubscriptionSettings {
        source_id: "grpc-crash".to_string(),
        query_id: "q1".to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_sequence: None,
        request_position_handle: true,
        resume_from: Some(bytes::Bytes::from(3u64.to_be_bytes().to_vec())),
    };
    let resp = source.subscribe(settings).await.unwrap();
    let mut rx = resp.receiver;

    // Should replay events 4, 5
    let event4 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event4.sequence, 4);

    let event5 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event5.sequence, 5);

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_grpc_deprovision_removes_wal() {
    let config = GrpcSourceConfig {
        host: "127.0.0.1".to_string(),
        port: 19404,
        endpoint: None,
        timeout_ms: 5000,
        durability: Some(durability_config(
            true,
            10_000,
            CapacityPolicy::RejectIncoming,
        )),
    };
    let source = GrpcSource::new("grpc-deprov", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "grpc-deprov").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let count = wal.event_count("grpc-deprov").await.unwrap();
    assert_eq!(count, 0);

    source.stop().await.unwrap();
    source.deprovision().await.unwrap();

    let count = wal.event_count("grpc-deprov").await;
    assert!(count.is_err(), "WAL should be deleted after deprovision");
}

/// With durability **disabled**, the gRPC source must still stamp a
/// framework-assigned monotonic sequence on every emitted event (issue #828).
/// Before the migration to `dispatch_event`, task-emitted events left
/// `sequence = None` whenever the WAL was off.
#[tokio::test]
async fn test_grpc_sequence_stamped_without_durability() {
    // Ephemeral port so this can run in CI without colliding with other tests.
    let port = reserve_port();
    // durability: None → WAL off, so no source-supplied sequence.
    let config = GrpcSourceConfig {
        host: "127.0.0.1".to_string(),
        port,
        endpoint: None,
        timeout_ms: 5000,
        durability: None,
    };
    let source = GrpcSource::new("grpc-noseq", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "grpc-noseq").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Durability is off, so the source neither persists nor supports replay.
    assert!(!source.supports_replay());

    // Subscribe before submitting so the events are dispatched to us.
    let settings = SourceSubscriptionSettings {
        source_id: "grpc-noseq".to_string(),
        query_id: "q1".to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_sequence: None,
        request_position_handle: true,
        resume_from: None,
    };
    let resp = source.subscribe(settings).await.unwrap();
    let mut rx = resp.receiver;

    // Connect a real gRPC client and submit events over the wire.
    let mut client = connect_with_retry(port).await;

    let event_count = 5u64;
    for i in 1..=event_count {
        let response = client
            .submit_event(insert_node_request("grpc-noseq", &format!("n{i}")))
            .await
            .expect("submit_event RPC failed");
        assert!(response.into_inner().success);
    }

    // Every emitted event must carry a framework-stamped, strictly increasing
    // sequence (1, 2, 3, ...), even though the WAL is disabled.
    for expected_seq in 1..=event_count {
        let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for event")
            .expect("event stream closed unexpectedly");
        assert_eq!(
            event.sequence, expected_seq,
            "event {expected_seq} should carry a framework sequence with durability off"
        );
    }

    source.stop().await.unwrap();
}
