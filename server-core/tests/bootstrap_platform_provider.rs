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

//! Integration tests for platform bootstrap provider

use drasi_server_core::bootstrap::providers::PlatformBootstrapProvider;
use drasi_server_core::bootstrap::{BootstrapContext, BootstrapProvider};
use drasi_server_core::channels::BootstrapRequest;
use drasi_server_core::config::SourceConfig;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_platform_bootstrap_with_mock_server() {
    // Start mock HTTP server
    let mock_server = MockServer::start().await;

    // Mock response with JSON-NL stream
    let response_body = r#"{"id":"1","labels":["Person"],"properties":{"name":"Alice","age":30}}
{"id":"2","labels":["Person"],"properties":{"name":"Bob","age":25}}
{"id":"r1","labels":["KNOWS"],"properties":{"since":"2020"},"startId":"1","endId":"2"}
"#;

    Mock::given(method("POST"))
        .and(path("/subscription"))
        .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
        .mount(&mock_server)
        .await;

    // Create platform bootstrap provider
    let provider = PlatformBootstrapProvider::new(mock_server.uri(), 30).unwrap();

    // Create channel for receiving bootstrap elements
    let (tx, mut rx) = mpsc::channel(100);

    // Create source config
    let mut properties = HashMap::new();
    properties.insert(
        "query_api_url".to_string(),
        serde_json::json!(mock_server.uri()),
    );

    let source_config = Arc::new(SourceConfig {
        id: "test-source".to_string(),
        source_type: "platform".to_string(),
        auto_start: true,
        properties,
        bootstrap_provider: None,
    });

    // Create bootstrap context
    let context = BootstrapContext::new("test_server".to_string(), source_config, tx.clone(), "test-source".to_string());

    // Create bootstrap request
    let request = BootstrapRequest {
        query_id: "test-query".to_string(),
        node_labels: vec![], // Empty = match all
        relation_labels: vec![],
        request_id: "req-1".to_string(),
    };

    // Execute bootstrap
    let result = provider.bootstrap(request, &context).await;
    assert!(result.is_ok());

    let element_count = result.unwrap();
    assert_eq!(element_count, 3); // 2 nodes + 1 relation

    // Verify elements were sent via channel
    let mut received_count = 0;
    while let Ok(event) = rx.try_recv() {
        assert_eq!(event.source_id, "test-source");
        received_count += 1;
    }
    assert_eq!(received_count, 3);
}

#[tokio::test]
async fn test_platform_bootstrap_label_filtering() {
    // Start mock HTTP server
    let mock_server = MockServer::start().await;

    // Mock response with various labels
    let response_body = r#"{"id":"1","labels":["Person"],"properties":{"name":"Alice"}}
{"id":"2","labels":["Company"],"properties":{"name":"Acme Inc"}}
{"id":"3","labels":["Person"],"properties":{"name":"Bob"}}
"#;

    Mock::given(method("POST"))
        .and(path("/subscription"))
        .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
        .mount(&mock_server)
        .await;

    let provider = PlatformBootstrapProvider::new(mock_server.uri(), 30).unwrap();

    let (tx, mut rx) = mpsc::channel(100);

    let source_config = Arc::new(SourceConfig {
        id: "test-source".to_string(),
        source_type: "platform".to_string(),
        auto_start: true,
        properties: HashMap::new(),
        bootstrap_provider: None,
    });

    let context = BootstrapContext::new("test_server".to_string(), source_config, tx.clone(), "test-source".to_string());

    // Request only Person labels
    let request = BootstrapRequest {
        query_id: "test-query".to_string(),
        node_labels: vec!["Person".to_string()],
        relation_labels: vec![],
        request_id: "req-1".to_string(),
    };

    let result = provider.bootstrap(request, &context).await;
    assert!(result.is_ok());

    let element_count = result.unwrap();
    assert_eq!(element_count, 2); // Only 2 Person nodes, Company filtered out

    let mut received_count = 0;
    while let Ok(_) = rx.try_recv() {
        received_count += 1;
    }
    assert_eq!(received_count, 2);
}

#[tokio::test]
async fn test_platform_bootstrap_connection_failure() {
    // Create provider pointing to non-existent server
    let provider = PlatformBootstrapProvider::new("http://localhost:9999".to_string(), 1).unwrap();

    let (tx, _rx) = mpsc::channel(100);

    let source_config = Arc::new(SourceConfig {
        id: "test-source".to_string(),
        source_type: "platform".to_string(),
        auto_start: true,
        properties: HashMap::new(),
        bootstrap_provider: None,
    });

    let context = BootstrapContext::new("test_server".to_string(), source_config, tx.clone(), "test-source".to_string());

    let request = BootstrapRequest {
        query_id: "test-query".to_string(),
        node_labels: vec![],
        relation_labels: vec![],
        request_id: "req-1".to_string(),
    };

    // Should fail due to connection error
    let result = provider.bootstrap(request, &context).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_platform_bootstrap_malformed_json() {
    // Start mock HTTP server
    let mock_server = MockServer::start().await;

    // Mock response with some malformed JSON lines
    let response_body = r#"{"id":"1","labels":["Person"],"properties":{"name":"Alice"}}
this is not valid json
{"id":"2","labels":["Person"],"properties":{"name":"Bob"}}
"#;

    Mock::given(method("POST"))
        .and(path("/subscription"))
        .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
        .mount(&mock_server)
        .await;

    let provider = PlatformBootstrapProvider::new(mock_server.uri(), 30).unwrap();

    let (tx, mut rx) = mpsc::channel(100);

    let source_config = Arc::new(SourceConfig {
        id: "test-source".to_string(),
        source_type: "platform".to_string(),
        auto_start: true,
        properties: HashMap::new(),
        bootstrap_provider: None,
    });

    let context = BootstrapContext::new("test_server".to_string(), source_config, tx.clone(), "test-source".to_string());

    let request = BootstrapRequest {
        query_id: "test-query".to_string(),
        node_labels: vec![],
        relation_labels: vec![],
        request_id: "req-1".to_string(),
    };

    let result = provider.bootstrap(request, &context).await;
    assert!(result.is_ok());

    // Should have 2 elements (malformed line skipped)
    let element_count = result.unwrap();
    assert_eq!(element_count, 2);

    let mut received_count = 0;
    while let Ok(_) = rx.try_recv() {
        received_count += 1;
    }
    assert_eq!(received_count, 2);
}
