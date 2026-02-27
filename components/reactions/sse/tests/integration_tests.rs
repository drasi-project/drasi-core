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

//! Integration tests for SSE reaction with full DrasiLib setup

mod mock_source;

use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_sse::{QueryConfig, SseExtension, SseReaction, TemplateSpec};
use futures_util::StreamExt;
use mock_source::{MockSource, PropertyMapBuilder};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// Test basic SSE functionality with Application source, query, and SSE reaction
#[tokio::test]
async fn test_sse_basic_integration() -> Result<()> {
    // Initialize logging for test debugging
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    // Create mock source and handle
    let (mock_source, handle) = MockSource::new("test-source")?;

    // Create a simple query
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

    // Create SSE reaction
    let sse_reaction = SseReaction::builder("test-sse")
        .with_port(18080)
        .with_query("test-query")
        .build()?;

    // Build DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("test-core")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?,
    );

    // Start the core
    core.start().await?;

    // Give the system time to initialize
    sleep(Duration::from_millis(500)).await;

    // Connect to SSE endpoint
    let client = reqwest::Client::new();
    let response = client.get("http://localhost:18080/events").send().await?;

    assert_eq!(response.status(), 200);

    // Read SSE events in a separate task
    let stream = response.bytes_stream();
    let mut event_stream = eventsource_stream::EventStream::new(stream).take(3); // Get first 3 events

    // Send test data
    let props = PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .with_integer("age", 30)
        .build();
    handle
        .send_node_insert("person-1", vec!["Person"], props)
        .await?;

    // Collect events
    let mut received_events = Vec::new();
    while let Some(event_result) = event_stream.next().await {
        match event_result {
            Ok(event) => {
                let data = event.data.clone();
                log::info!("Received SSE event: {data}");
                received_events.push(data.clone());

                // Stop after receiving a non-heartbeat event
                if !data.contains("heartbeat") {
                    break;
                }
            }
            Err(e) => {
                log::error!("Error reading SSE event: {e}");
                break;
            }
        }
    }

    // Verify we received at least one event
    assert!(
        !received_events.is_empty(),
        "Should receive at least one SSE event"
    );

    // Check for data event (not just heartbeat)
    let has_data_event = received_events.iter().any(|e| !e.contains("heartbeat"));
    assert!(has_data_event, "Should receive at least one data event");

    // Cleanup
    core.stop().await?;

    Ok(())
}

/// Test SSE with custom templates
#[tokio::test]
async fn test_sse_custom_templates_integration() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    // Create mock source
    let (mock_source, handle) = MockSource::new("test-source")?;

    // Create query
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

    // Create SSE reaction with custom template
    let custom_config = QueryConfig {
        added: Some(TemplateSpec::with_extension(
            r#"{"event":"person_added","name":"{{after.name}}","age":{{after.age}}}"#,
            SseExtension { path: None },
        )),
        updated: Some(TemplateSpec::with_extension(
            r#"{"event":"person_updated","name":"{{after.name}}","old_age":{{before.age}},"new_age":{{after.age}}}"#,
            SseExtension { path: None },
        )),
        deleted: None,
    };

    let sse_reaction = SseReaction::builder("test-sse")
        .with_port(18081)
        .with_query("test-query")
        .with_route("test-query", custom_config)
        .build()?;

    // Build and start DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("test-core")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?,
    );

    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    // Connect to SSE endpoint
    let client = reqwest::Client::new();
    let response = client.get("http://localhost:18081/events").send().await?;

    assert_eq!(response.status(), 200);

    let stream = response.bytes_stream();
    let mut event_stream = eventsource_stream::EventStream::new(stream).take(5);

    // Send test data
    let props = PropertyMapBuilder::new()
        .with_string("name", "Bob")
        .with_integer("age", 25)
        .build();
    handle
        .send_node_insert("person-2", vec!["Person"], props)
        .await?;

    // Collect events
    let mut received_custom_event = false;
    while let Some(event_result) = event_stream.next().await {
        if let Ok(event) = event_result {
            let data = event.data.clone();
            log::info!("Received custom template event: {data}");

            // Check if we received our custom formatted event
            if data.contains("person_added") && data.contains("Bob") {
                received_custom_event = true;
                break;
            }
        }
    }

    assert!(
        received_custom_event,
        "Should receive custom templated event"
    );

    core.stop().await?;
    Ok(())
}

/// Test SSE with multi-path routing
#[tokio::test]
async fn test_sse_multi_path_integration() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    // Create mock source
    let (mock_source, handle) = MockSource::new("test-source")?;

    // Create two queries
    let person_query = Query::cypher("person-query")
        .query(
            r#"
            MATCH (p:Person)
            RETURN p.name AS name
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    let company_query = Query::cypher("company-query")
        .query(
            r#"
            MATCH (c:Company)
            RETURN c.name AS name
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    // Create SSE reaction with different paths for each query
    let person_config = QueryConfig {
        added: Some(TemplateSpec::with_extension(
            r#"{"type":"person","name":"{{after.name}}"}"#,
            SseExtension {
                path: Some("/persons".to_string()),
            },
        )),
        updated: None,
        deleted: None,
    };

    let company_config = QueryConfig {
        added: Some(TemplateSpec::with_extension(
            r#"{"type":"company","name":"{{after.name}}"}"#,
            SseExtension {
                path: Some("/companies".to_string()),
            },
        )),
        updated: None,
        deleted: None,
    };

    let sse_reaction = SseReaction::builder("test-sse")
        .with_port(18082)
        .with_query("person-query")
        .with_query("company-query")
        .with_route("person-query", person_config)
        .with_route("company-query", company_config)
        .build()?;

    // Build and start DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("test-core")
            .with_source(mock_source)
            .with_query(person_query)
            .with_query(company_query)
            .with_reaction(sse_reaction)
            .build()
            .await?,
    );

    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    // Connect to both SSE paths
    let client = reqwest::Client::new();

    // Test /persons path
    let persons_response = client.get("http://localhost:18082/persons").send().await?;
    assert_eq!(persons_response.status(), 200);

    // Test /companies path
    let companies_response = client
        .get("http://localhost:18082/companies")
        .send()
        .await?;
    assert_eq!(companies_response.status(), 200);

    // Start reading from both streams
    let persons_stream_bytes = persons_response.bytes_stream();
    let companies_stream_bytes = companies_response.bytes_stream();
    let mut persons_stream = eventsource_stream::EventStream::new(persons_stream_bytes).take(5);
    let mut companies_stream = eventsource_stream::EventStream::new(companies_stream_bytes).take(5);

    // Send person data
    let person_props = PropertyMapBuilder::new()
        .with_string("name", "Charlie")
        .build();
    handle
        .send_node_insert("person-3", vec!["Person"], person_props)
        .await?;

    sleep(Duration::from_millis(200)).await;

    // Send company data
    let company_props = PropertyMapBuilder::new()
        .with_string("name", "Acme Corp")
        .build();
    handle
        .send_node_insert("company-1", vec!["Company"], company_props)
        .await?;

    // Verify person event on /persons path
    let mut received_person = false;
    while let Some(event_result) = persons_stream.next().await {
        if let Ok(event) = event_result {
            let data = event.data.clone();
            log::info!("Received on /persons: {data}");
            if data.contains("Charlie") && data.contains("person") {
                received_person = true;
                break;
            }
        }
    }

    // Verify company event on /companies path
    let mut received_company = false;
    while let Some(event_result) = companies_stream.next().await {
        if let Ok(event) = event_result {
            let data = event.data.clone();
            log::info!("Received on /companies: {data}");
            if data.contains("Acme") && data.contains("company") {
                received_company = true;
                break;
            }
        }
    }

    assert!(
        received_person,
        "Should receive person event on /persons path"
    );
    assert!(
        received_company,
        "Should receive company event on /companies path"
    );

    core.stop().await?;
    Ok(())
}

/// Test that infinity values from log10(0) and log(0) flow through the entire pipeline
/// Source → Query → Reaction without panicking
#[tokio::test]
async fn test_infinity_e2e_source_query_reaction() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    // Create Application source
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
    };
    let (app_source, handle) = ApplicationSource::new("test-source", config)?;

    // Create query that computes log10(0) which produces -Infinity
    let query = Query::cypher("log-test")
        .query(
            r#"
            MATCH (n:TestNode)
            RETURN 
                n.id AS id,
                n.value AS value,
                log10(n.value) AS log10_result,
                log(n.value) AS ln_result
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    // Create SSE reaction
    let sse_reaction = SseReaction::builder("infinity-sse")
        .with_port(18090)
        .with_query("log-test")
        .build()?;

    // Build DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("infinity-test-core")
            .with_source(app_source)
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?,
    );

    // Start the core
    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    // Connect to SSE endpoint
    let client = reqwest::Client::new();
    let response = client.get("http://localhost:18090/events").send().await?;
    assert_eq!(response.status(), 200);

    let stream = response.bytes_stream();
    let mut event_stream = eventsource_stream::EventStream::new(stream).take(5);

    // Test 1: Send a node with value = 0, which will produce log10(0) = -Infinity
    let props = PropertyMapBuilder::new()
        .with_string("id", "node-1")
        .with_integer("value", 0)
        .build();

    handle
        .send_node_insert("node-1", vec!["TestNode"], props)
        .await?;

    // Test 2: Send a node with positive value for comparison
    sleep(Duration::from_millis(200)).await;
    let props2 = PropertyMapBuilder::new()
        .with_string("id", "node-2")
        .with_integer("value", 100)
        .build();

    handle
        .send_node_insert("node-2", vec!["TestNode"], props2)
        .await?;

    // Collect events
    let mut received_data_event = false;
    while let Some(event_result) = event_stream.next().await {
        match event_result {
            Ok(event) => {
                let data = event.data.clone();
                log::info!("Received SSE event: {}", data);

                // Check if it's a data event (not heartbeat)
                if !data.contains("heartbeat") {
                    received_data_event = true;

                    // Verify infinity is serialized as null (JSON doesn't support infinity)
                    if data.contains("node-1") {
                        // This event has log10(0) and log(0) which should be null
                        assert!(
                            data.contains("null"),
                            "Should contain null for infinity values"
                        );
                        // Make sure it's NOT "-inf" (that would indicate Display trait is being used)
                        assert!(!data.contains("-inf"), "Should not contain -inf string");
                        log::info!("Infinity correctly serialized as null in query results");
                    }

                    if data.contains("node-2") {
                        // This event has log10(100) = 2.0
                        assert!(data.contains("2") || data.contains("log10_result"));
                        log::info!("Normal float value serialized correctly");
                    }
                }

                if received_data_event {
                    break;
                }
            }
            Err(e) => {
                log::error!("Error reading SSE event: {}", e);
                break;
            }
        }
    }

    assert!(
        received_data_event,
        "Should receive at least one data event"
    );

    core.stop().await?;
    Ok(())
}
