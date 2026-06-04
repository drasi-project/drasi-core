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

//! WAL (Write-Ahead Log) integration tests for the HTTP source.
//!
//! These tests verify durable event persistence, crash recovery, and replay
//! functionality using the redb WAL backend with the HTTP source.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ChangeReceiver;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::wal::{CapacityPolicy, WalProvider};
use drasi_lib::Source;
use drasi_source_http::HttpSourceBuilder;
use drasi_wal_redb::RedbWalProvider;
use reqwest::Client;
use tempfile::TempDir;

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

/// Helper: create subscription settings (no resume)
fn fresh_settings(source_id: &str) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source_id.to_string(),
        query_id: "test-query".to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        request_position_handle: true,
        resume_from: None,
        last_sequence: None,
    }
}

/// Helper: create subscription settings with resume position
fn resume_settings(source_id: &str, resume_seq: u64) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source_id.to_string(),
        query_id: "test-query-resume".to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        request_position_handle: true,
        resume_from: Some(bytes::Bytes::from(resume_seq.to_be_bytes().to_vec())),
        last_sequence: Some(resume_seq),
    }
}

/// Helper: subscribe with default settings (no resume)
async fn subscribe_fresh(
    source: &dyn Source,
    source_id: &str,
) -> Box<dyn ChangeReceiver<drasi_lib::channels::events::SourceEventWrapper>> {
    let resp = source.subscribe(fresh_settings(source_id)).await.unwrap();
    resp.receiver
}

/// Helper: POST an event to the HTTP source
async fn post_event(client: &Client, port: u16, source_id: &str, node_id: &str, name: &str) {
    let body = serde_json::json!({
        "operation": "insert",
        "element": {
            "type": "node",
            "id": node_id,
            "labels": ["Person"],
            "properties": {
                "name": name
            }
        }
    });

    let url = format!("http://127.0.0.1:{port}/sources/{source_id}/events");
    let resp = client.post(&url).json(&body).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "POST failed with status {}",
        resp.status()
    );
}

/// Helper: POST and return status code
async fn post_event_status(
    client: &Client,
    port: u16,
    source_id: &str,
    node_id: &str,
    name: &str,
) -> u16 {
    let body = serde_json::json!({
        "operation": "insert",
        "element": {
            "type": "node",
            "id": node_id,
            "labels": ["Person"],
            "properties": {
                "name": name
            }
        }
    });

    let url = format!("http://127.0.0.1:{port}/sources/{source_id}/events");
    let resp = client.post(&url).json(&body).send().await.unwrap();
    resp.status().as_u16()
}

// ============================================================
// HTTP Source WAL Tests
// ============================================================

#[tokio::test]
#[ignore]
async fn test_http_wal_disabled_no_persistence() {
    let port = 19301u16;
    let source = HttpSourceBuilder::new("http-disabled")
        .with_host("127.0.0.1")
        .with_port(port)
        .build()
        .unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "http-disabled").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(!source.supports_replay());

    let count = wal.event_count("http-disabled").await;
    assert!(count.is_err(), "WAL should not be registered when disabled");

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_http_wal_enabled_events_persisted() {
    let port = 19302u16;
    let source = HttpSourceBuilder::new("http-persist")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_durability(durability_config(
            true,
            10_000,
            CapacityPolicy::RejectIncoming,
        ))
        .build()
        .unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "http-persist").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(source.supports_replay());

    let mut rx = subscribe_fresh(&source, "http-persist").await;

    let client = Client::new();
    post_event(&client, port, "http-persist", "n1", "Alice").await;

    let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.sequence.unwrap(), 1);
    assert!(event.source_position.is_some());

    let count = wal.event_count("http-persist").await.unwrap();
    assert_eq!(count, 1);

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_http_capacity_exhaustion_returns_503() {
    let port = 19303u16;
    // redb WAL requires max_events >= 16
    let source = HttpSourceBuilder::new("http-cap")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_durability(durability_config(true, 16, CapacityPolicy::RejectIncoming))
        .build()
        .unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "http-cap").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut rx = subscribe_fresh(&source, "http-cap").await;

    let client = Client::new();

    // Fill to capacity (16 events)
    for i in 1..=16 {
        post_event(
            &client,
            port,
            "http-cap",
            &format!("n{i}"),
            &format!("V{i}"),
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap();
    }

    // 17th event should return 503
    let status = post_event_status(&client, port, "http-cap", "n17", "C").await;
    assert_eq!(
        status, 503,
        "Expected 503 Service Unavailable when WAL is full"
    );

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_http_crash_recovery_and_replay() {
    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    let port = 19304u16;

    // First lifecycle — write events
    {
        let source = HttpSourceBuilder::new("http-crash")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_durability(durability_config(
                true,
                10_000,
                CapacityPolicy::RejectIncoming,
            ))
            .build()
            .unwrap();

        init_source_with_wal(&source, wal.clone(), "http-crash").await;
        source.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut rx = subscribe_fresh(&source, "http-crash").await;

        let client = Client::new();
        for i in 1..=5 {
            post_event(
                &client,
                port,
                "http-crash",
                &format!("n{i}"),
                &format!("Name{i}"),
            )
            .await;
            let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap();
        }

        let head = wal.head_sequence("http-crash").await.unwrap();
        assert_eq!(head, 5);

        source.stop().await.unwrap();
    }

    // Second lifecycle — resume and replay
    {
        let port2 = 19305u16;
        let source = HttpSourceBuilder::new("http-crash")
            .with_host("127.0.0.1")
            .with_port(port2)
            .with_durability(durability_config(
                true,
                10_000,
                CapacityPolicy::RejectIncoming,
            ))
            .build()
            .unwrap();

        init_source_with_wal(&source, wal.clone(), "http-crash").await;
        source.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Subscribe with resume_from = seq 3 → should replay events 4, 5
        let resp = source
            .subscribe(resume_settings("http-crash", 3))
            .await
            .unwrap();
        let mut rx = resp.receiver;

        let event4 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event4.sequence.unwrap(), 4);

        let event5 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event5.sequence.unwrap(), 5);

        // New event should be seq 6
        let client = Client::new();
        post_event(&client, port2, "http-crash", "n6", "Name6").await;
        let event6 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event6.sequence.unwrap(), 6);

        source.stop().await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn test_http_deprovision_removes_wal() {
    let port = 19306u16;
    let source = HttpSourceBuilder::new("http-deprov")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_durability(durability_config(
            true,
            10_000,
            CapacityPolicy::RejectIncoming,
        ))
        .build()
        .unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "http-deprov").await;

    source.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut rx = subscribe_fresh(&source, "http-deprov").await;

    let client = Client::new();
    post_event(&client, port, "http-deprov", "n1", "X").await;
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap();

    assert!(wal.event_count("http-deprov").await.unwrap() > 0);

    source.stop().await.unwrap();
    source.deprovision().await.unwrap();

    let count = wal.event_count("http-deprov").await;
    assert!(count.is_err(), "WAL should be deleted after deprovision");
}
