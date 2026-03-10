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
use serde_json;
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

/// Test that aggregation query results are delivered as SSE events in default format.
///
/// When no custom routes are configured, the SSE reaction sends all results
/// (including Aggregation variants) in the default JSON format.
#[tokio::test]
async fn test_sse_aggregation_default_format() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let (mock_source, handle) = MockSource::new("test-source")?;

    // Aggregation query using COUNT
    let query = Query::cypher("agg-default-query")
        .query(
            r#"
            MATCH (s:Sensor)
            RETURN count(s) AS sensor_count
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    // No custom routes - uses default format
    let sse_reaction = SseReaction::builder("test-sse-agg-default")
        .with_port(18083)
        .with_query("agg-default-query")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("test-core-agg-default")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?,
    );

    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    let client = reqwest::Client::new();
    let response = client.get("http://localhost:18083/events").send().await?;
    assert_eq!(response.status(), 200);

    let stream = response.bytes_stream();
    let mut event_stream = eventsource_stream::EventStream::new(stream).take(5);

    // Insert a sensor node - should trigger aggregation result
    let props = PropertyMapBuilder::new()
        .with_string("location", "building-a")
        .with_float("temperature", 22.5)
        .build();
    handle
        .send_node_insert("sensor-1", vec!["Sensor"], props)
        .await?;

    // Collect events with a timeout
    let mut received_aggregation = false;
    let collect_result = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event_result) = event_stream.next().await {
            if let Ok(event) = event_result {
                let data = event.data.clone();
                log::info!("Received SSE event: {data}");

                // Default format includes the serialized ResultDiff with type "aggregation"
                if data.contains("aggregation") && data.contains("sensor_count") {
                    received_aggregation = true;
                    break;
                }
            }
        }
    })
    .await;

    assert!(
        collect_result.is_ok(),
        "Timed out waiting for SSE aggregation event"
    );
    assert!(
        received_aggregation,
        "Should receive aggregation event with sensor_count"
    );

    core.stop().await?;
    Ok(())
}

/// Test that aggregation results are routed through the `updated` template spec.
///
/// Aggregation events use the `updated` route configuration, allowing users
/// to format aggregation output with custom Handlebars templates.
#[tokio::test]
async fn test_sse_aggregation_custom_template() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let (mock_source, handle) = MockSource::new("test-source")?;

    let query = Query::cypher("agg-template-query")
        .query(
            r#"
            MATCH (s:Sensor)
            RETURN count(s) AS sensor_count
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    // Configure an "updated" template — aggregation events route through it
    let custom_config = QueryConfig {
        added: None,
        updated: Some(TemplateSpec::with_extension(
            r#"{"event":"aggregation_update","sensor_count":{{after.sensor_count}}}"#,
            SseExtension { path: None },
        )),
        deleted: None,
    };

    let sse_reaction = SseReaction::builder("test-sse-agg-template")
        .with_port(18084)
        .with_query("agg-template-query")
        .with_route("agg-template-query", custom_config)
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("test-core-agg-template")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?,
    );

    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    let client = reqwest::Client::new();
    let response = client.get("http://localhost:18084/events").send().await?;
    assert_eq!(response.status(), 200);

    let stream = response.bytes_stream();
    let mut event_stream = eventsource_stream::EventStream::new(stream).take(5);

    let props = PropertyMapBuilder::new()
        .with_string("location", "building-a")
        .with_float("temperature", 22.5)
        .build();
    handle
        .send_node_insert("sensor-1", vec!["Sensor"], props)
        .await?;

    let mut received_custom_event = false;
    let collect_result = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event_result) = event_stream.next().await {
            if let Ok(event) = event_result {
                let data = event.data.clone();
                log::info!("Received custom aggregation event: {data}");

                if data.contains("aggregation_update") && data.contains("sensor_count") {
                    // Verify the custom template was applied
                    let parsed: serde_json::Value =
                        serde_json::from_str(&data).expect("Event data should be valid JSON");
                    assert_eq!(parsed["event"], "aggregation_update");
                    assert_eq!(parsed["sensor_count"], 1);
                    received_custom_event = true;
                    break;
                }
            }
        }
    })
    .await;

    assert!(
        collect_result.is_ok(),
        "Timed out waiting for custom aggregation SSE event"
    );
    assert!(
        received_custom_event,
        "Should receive custom templated aggregation event"
    );

    core.stop().await?;
    Ok(())
}

/// Test aggregation before/after values across multiple inserts.
///
/// The first insert produces `Aggregation { before: { sensor_count: 0 }, after: { sensor_count: 1 } }`.
/// The second insert produces `Aggregation { before: { sensor_count: 1 }, after: { sensor_count: 2 } }`.
#[tokio::test]
async fn test_sse_aggregation_before_after() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let (mock_source, handle) = MockSource::new("test-source")?;

    let query = Query::cypher("agg-ba-query")
        .query(
            r#"
            MATCH (s:Sensor)
            RETURN count(s) AS sensor_count
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    // No custom routes - default format includes the full ResultDiff JSON
    let sse_reaction = SseReaction::builder("test-sse-agg-ba")
        .with_port(18085)
        .with_query("agg-ba-query")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("test-core-agg-ba")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?,
    );

    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    let client = reqwest::Client::new();
    let response = client.get("http://localhost:18085/events").send().await?;
    assert_eq!(response.status(), 200);

    let stream = response.bytes_stream();
    let mut event_stream = eventsource_stream::EventStream::new(stream).take(10);

    // Insert first sensor
    let props1 = PropertyMapBuilder::new()
        .with_string("location", "building-a")
        .with_float("temperature", 22.5)
        .build();
    handle
        .send_node_insert("sensor-1", vec!["Sensor"], props1)
        .await?;

    // Give time for first event to be processed
    sleep(Duration::from_millis(500)).await;

    // Insert second sensor
    let props2 = PropertyMapBuilder::new()
        .with_string("location", "building-b")
        .with_float("temperature", 24.0)
        .build();
    handle
        .send_node_insert("sensor-2", vec!["Sensor"], props2)
        .await?;

    // Collect aggregation events
    let mut aggregation_events: Vec<serde_json::Value> = Vec::new();
    let collect_result = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event_result) = event_stream.next().await {
            if let Ok(event) = event_result {
                let data = event.data.clone();
                log::info!("Received SSE event: {data}");

                if data.contains("aggregation") && data.contains("sensor_count") {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&data) {
                        aggregation_events.push(parsed);
                        if aggregation_events.len() >= 2 {
                            break;
                        }
                    }
                }
            }
        }
    })
    .await;

    assert!(
        collect_result.is_ok(),
        "Timed out waiting for aggregation SSE events"
    );
    assert!(
        aggregation_events.len() >= 2,
        "Should receive at least 2 aggregation events, got {}",
        aggregation_events.len()
    );

    // First event: before has the initial aggregation state (count=0)
    let first_results = aggregation_events[0]["results"]
        .as_array()
        .expect("results should be an array");
    let first_agg = first_results
        .iter()
        .find(|r| r["type"] == "aggregation")
        .expect("First event should contain aggregation result");
    assert_eq!(
        first_agg["before"]["sensor_count"], 0,
        "First aggregation before should have sensor_count=0"
    );
    assert_eq!(first_agg["after"]["sensor_count"], 1);

    // Second event: before should contain previous state (count=1)
    let second_results = aggregation_events[1]["results"]
        .as_array()
        .expect("results should be an array");
    let second_agg = second_results
        .iter()
        .find(|r| r["type"] == "aggregation")
        .expect("Second event should contain aggregation result");
    assert_eq!(
        second_agg["before"]["sensor_count"], 1,
        "Second aggregation before should have sensor_count=1"
    );
    assert_eq!(second_agg["after"]["sensor_count"], 2);

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
