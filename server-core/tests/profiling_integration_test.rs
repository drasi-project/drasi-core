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

use drasi_server_core::{
    channels::{QueryResult, SourceEvent, SourceEventWrapper},
    profiling::ProfilingMetadata,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_profiling_end_to_end_flow() {
    // Create a test source event with profiling
    let mut profiling = ProfilingMetadata::new();
    let start_time = drasi_server_core::profiling::timestamp_ns();
    profiling.source_send_ns = Some(start_time);

    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", "node1"),
            labels: Arc::from(vec![Arc::from("TestNode")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::new(),
    };

    let source_change = SourceChange::Insert { element };

    let event_wrapper = SourceEventWrapper::with_profiling(
        "test_source".to_string(),
        SourceEvent::Change(source_change),
        chrono::Utc::now(),
        profiling.clone(),
    );

    // Verify source profiling is captured
    assert!(event_wrapper.profiling.is_some());
    let event_profiling = event_wrapper.profiling.as_ref().unwrap();
    assert!(event_profiling.source_send_ns.is_some());
    assert_eq!(event_profiling.source_send_ns.unwrap(), start_time);

    // Simulate query receiving the event
    let mut query_profiling = profiling.clone();
    query_profiling.query_receive_ns = Some(drasi_server_core::profiling::timestamp_ns());

    // Small delay to ensure timestamps differ
    tokio::time::sleep(tokio::time::Duration::from_micros(10)).await;

    // Simulate query processing
    query_profiling.query_core_call_ns = Some(drasi_server_core::profiling::timestamp_ns());
    tokio::time::sleep(tokio::time::Duration::from_micros(10)).await;
    query_profiling.query_core_return_ns = Some(drasi_server_core::profiling::timestamp_ns());
    query_profiling.query_send_ns = Some(drasi_server_core::profiling::timestamp_ns());

    // Create query result with profiling
    let query_result = QueryResult::with_profiling(
        "test_query".to_string(),
        chrono::Utc::now(),
        vec![],
        std::collections::HashMap::new(),
        query_profiling.clone(),
    );

    // Verify query profiling timestamps
    assert!(query_result.profiling.is_some());
    let result_profiling = query_result.profiling.as_ref().unwrap();
    assert!(result_profiling.query_receive_ns.is_some());
    assert!(result_profiling.query_core_call_ns.is_some());
    assert!(result_profiling.query_core_return_ns.is_some());
    assert!(result_profiling.query_send_ns.is_some());

    // Verify timestamps are in order
    assert!(result_profiling.source_send_ns.unwrap() < result_profiling.query_receive_ns.unwrap());
    assert!(result_profiling.query_receive_ns.unwrap() <= result_profiling.query_core_call_ns.unwrap());
    assert!(result_profiling.query_core_call_ns.unwrap() < result_profiling.query_core_return_ns.unwrap());
    assert!(result_profiling.query_core_return_ns.unwrap() <= result_profiling.query_send_ns.unwrap());

    // Simulate reaction receiving and processing
    let mut reaction_profiling = query_profiling;
    reaction_profiling.reaction_receive_ns = Some(drasi_server_core::profiling::timestamp_ns());
    tokio::time::sleep(tokio::time::Duration::from_micros(10)).await;
    reaction_profiling.reaction_complete_ns = Some(drasi_server_core::profiling::timestamp_ns());

    // Verify reaction profiling
    assert!(reaction_profiling.reaction_receive_ns.is_some());
    assert!(reaction_profiling.reaction_complete_ns.is_some());
    assert!(reaction_profiling.query_send_ns.unwrap() <= reaction_profiling.reaction_receive_ns.unwrap());
    assert!(reaction_profiling.reaction_receive_ns.unwrap() < reaction_profiling.reaction_complete_ns.unwrap());

    // Verify we can calculate end-to-end latency
    let total_latency = reaction_profiling.reaction_complete_ns.unwrap()
        - reaction_profiling.source_send_ns.unwrap();
    assert!(total_latency > 0);
}

#[tokio::test]
async fn test_profiling_preserves_through_channels() {
    // Create profiling metadata
    let mut profiling = ProfilingMetadata::new();
    profiling.source_send_ns = Some(drasi_server_core::profiling::timestamp_ns());

    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", "node1"),
            labels: Arc::from(vec![Arc::from("TestNode")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::new(),
    };

    let source_change = SourceChange::Insert { element };

    let event_wrapper = SourceEventWrapper::with_profiling(
        "test_source".to_string(),
        SourceEvent::Change(source_change),
        chrono::Utc::now(),
        profiling.clone(),
    );

    // Send through channel
    let (tx, mut rx) = mpsc::channel(10);
    tx.send(event_wrapper.clone()).await.unwrap();

    // Receive from channel
    let received = rx.recv().await.unwrap();

    // Verify profiling is preserved
    assert!(received.profiling.is_some());
    assert_eq!(
        received.profiling.as_ref().unwrap().source_send_ns,
        profiling.source_send_ns
    );
}

#[tokio::test]
async fn test_profiling_optional_timestamps() {
    // Create profiling with only some timestamps
    let mut profiling = ProfilingMetadata::new();
    profiling.source_send_ns = Some(100);
    profiling.query_receive_ns = Some(200);
    // Leave other timestamps as None

    // Verify we can still work with partial profiling data
    assert!(profiling.source_send_ns.is_some());
    assert!(profiling.query_receive_ns.is_some());
    assert!(profiling.query_core_call_ns.is_none());
    assert!(profiling.reaction_complete_ns.is_none());

    // Can calculate partial latency
    if let (Some(send), Some(recv)) = (profiling.source_send_ns, profiling.query_receive_ns) {
        let latency = recv - send;
        assert_eq!(latency, 100);
    }
}

#[test]
fn test_profiling_elapsed_calculation() {
    let start = 1000000000; // 1 second in nanoseconds
    let end = 1500000000;   // 1.5 seconds

    let elapsed = end - start;
    assert_eq!(elapsed, 500000000); // 0.5 seconds
}

#[tokio::test]
async fn test_query_result_profiling_serialization() {
    // Create query result with profiling
    let mut profiling = ProfilingMetadata::new();
    profiling.source_send_ns = Some(1000);
    profiling.query_receive_ns = Some(2000);
    profiling.query_send_ns = Some(3000);

    let query_result = QueryResult::with_profiling(
        "test_query".to_string(),
        chrono::Utc::now(),
        vec![serde_json::json!({"test": "data"})],
        std::collections::HashMap::new(),
        profiling.clone(),
    );

    // Serialize to JSON
    let json = serde_json::to_string(&query_result).unwrap();
    assert!(json.contains("profiling"));

    // Deserialize from JSON
    let deserialized: QueryResult = serde_json::from_str(&json).unwrap();
    assert!(deserialized.profiling.is_some());
    assert_eq!(
        deserialized.profiling.as_ref().unwrap().source_send_ns,
        Some(1000)
    );
}

#[tokio::test]
async fn test_profiling_without_metadata() {
    // Create event without profiling
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", "node1"),
            labels: Arc::from(vec![Arc::from("TestNode")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::new(),
    };

    let source_change = SourceChange::Insert { element };

    let event_wrapper = SourceEventWrapper::new(
        "test_source".to_string(),
        SourceEvent::Change(source_change),
        chrono::Utc::now(),
    );

    // Verify profiling is None
    assert!(event_wrapper.profiling.is_none());
}
