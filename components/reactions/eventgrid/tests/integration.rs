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

//! End-to-end tests for the Event Grid reaction.
//!
//! Event Grid is a **protocol target**: the reaction POSTs event batches to the
//! topic's HTTP endpoint. A local `wiremock` server stands in for the real
//! topic and captures the posted payloads for assertions — no Docker required.
//!
//! Run with: `cargo test -p drasi-reaction-eventgrid --test integration -- --ignored --nocapture`

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_eventgrid::{EventGridReaction, EventGridSchema, OutputFormat};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use serde_json::Value;
use std::collections::HashMap;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = "test-access-key";

/// Brief pause after `core.start()` to let the source→query→reaction pipeline
/// wire up before the first change is sent. Kept small because request arrival
/// is awaited precisely by [`wait_for_requests`]; this only covers subscription
/// setup, not event propagation.
const PIPELINE_WARMUP: Duration = Duration::from_millis(100);

fn make_source(
    id: &str,
) -> (
    ApplicationSource,
    drasi_source_application::ApplicationSourceHandle,
) {
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: None,
    };
    ApplicationSource::new(id, config).expect("create application source")
}

/// Poll until `expected` requests have been observed, panicking (with the
/// observed count) if the deadline elapses first.
async fn wait_for_requests(server: &MockServer, expected: usize, max_ms: u64) {
    let deadline = std::time::Instant::now() + Duration::from_millis(max_ms);
    loop {
        let count = server.received_requests().await.unwrap_or_default().len();
        if count >= expected {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out after {max_ms}ms waiting for {expected} request(s); observed {count}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Concatenate all captured request bodies into a single JSON array of events.
async fn all_events(server: &MockServer) -> Vec<Value> {
    let reqs = server.received_requests().await.unwrap_or_default();
    reqs.iter()
        .filter_map(|req| serde_json::from_slice::<Value>(&req.body).ok())
        .filter_map(|body| body.as_array().cloned())
        .flatten()
        .collect()
}

#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn eventgrid_end_to_end_unpacked_cloudevents() -> Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/events"))
        .and(header("aeg-sas-key", TEST_KEY))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (source, handle) = make_source("test-source");
    let query = Query::cypher("people-query")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source("test-source")
        .auto_start(true)
        .build();
    let reaction = EventGridReaction::builder("eventgrid-e2e")
        .with_endpoint(format!("{}/api/events", server.uri()))
        .with_allow_http(true)
        .with_access_key(TEST_KEY)
        .with_schema(EventGridSchema::CloudEvents)
        .with_format(OutputFormat::Unpacked)
        .with_query("people-query")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("eventgrid-e2e-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    core.start().await?;
    tokio::time::sleep(PIPELINE_WARMUP).await;

    // ---- INSERT ----
    let props = PropertyMapBuilder::new()
        .with_string("name", "Ada")
        .with_integer("age", 36)
        .build();
    handle
        .send_node_insert("person-1", vec!["Person"], props)
        .await?;
    wait_for_requests(&server, 1, 5000).await;

    {
        let events = all_events(&server).await;
        let insert = events.last().expect("insert event");
        assert_eq!(insert["type"], "Drasi.ChangeEvent", "cloudevents type");
        assert_eq!(insert["specversion"], "1.0");
        assert_eq!(insert["subject"], "people-query");
        assert_eq!(insert["data"]["op"], "i", "insert op");
        assert_eq!(insert["data"]["payload"]["after"]["name"], "Ada");
    }

    // ---- UPDATE ----
    let props = PropertyMapBuilder::new()
        .with_string("name", "Ada Lovelace")
        .with_integer("age", 37)
        .build();
    handle
        .send_node_update("person-1", vec!["Person"], props)
        .await?;
    wait_for_requests(&server, 2, 5000).await;

    {
        let events = all_events(&server).await;
        let update = events
            .iter()
            .find(|e| e["data"]["op"] == "u")
            .expect("update event");
        assert!(update["data"]["payload"]["before"].is_object());
        assert_eq!(update["data"]["payload"]["after"]["name"], "Ada Lovelace");
    }

    // ---- DELETE ----
    handle.send_delete("person-1", vec!["Person"]).await?;
    wait_for_requests(&server, 3, 5000).await;

    {
        let events = all_events(&server).await;
        let delete = events
            .iter()
            .find(|e| e["data"]["op"] == "d")
            .expect("delete event");
        assert!(delete["data"]["payload"]["before"].is_object());
    }

    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn eventgrid_end_to_end_eventgrid_schema_insert() -> Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/events"))
        .and(header("aeg-sas-key", TEST_KEY))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (source, handle) = make_source("test-source");
    let query = Query::cypher("people-query")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build();
    let reaction = EventGridReaction::builder("eventgrid-native")
        .with_endpoint(format!("{}/api/events", server.uri()))
        .with_allow_http(true)
        .with_access_key(TEST_KEY)
        .with_schema(EventGridSchema::EventGrid)
        .with_format(OutputFormat::Unpacked)
        .with_query("people-query")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("eventgrid-native-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    core.start().await?;
    tokio::time::sleep(PIPELINE_WARMUP).await;

    let props = PropertyMapBuilder::new()
        .with_string("name", "Grace")
        .build();
    handle
        .send_node_insert("person-1", vec!["Person"], props)
        .await?;
    wait_for_requests(&server, 1, 5000).await;

    let events = all_events(&server).await;
    let insert = events.last().expect("insert event");
    // Native EventGrid schema field names.
    assert_eq!(insert["eventType"], "Drasi.ChangeEvent");
    assert_eq!(insert["dataVersion"], "1");
    assert_eq!(insert["subject"], "people-query");
    assert_eq!(insert["data"]["op"], "i");
    assert_eq!(insert["data"]["payload"]["after"]["name"], "Grace");

    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn eventgrid_template_metadata_becomes_extension_attributes() -> Result<()> {
    use drasi_reaction_eventgrid::{
        EventGridQueryConfig, EventGridTemplateExt, EventGridTemplateSpec,
    };

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/events"))
        .and(header("aeg-sas-key", TEST_KEY))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (source, handle) = make_source("test-source");
    let query = Query::cypher("people-query")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build();

    let template = EventGridQueryConfig {
        added: Some(EventGridTemplateSpec::with_extension(
            r#"{ "greeting": "hello {{after.name}}" }"#,
            EventGridTemplateExt {
                metadata: HashMap::from([("category".to_string(), "people".to_string())]),
            },
        )),
        ..Default::default()
    };

    let reaction = EventGridReaction::builder("eventgrid-template")
        .with_endpoint(format!("{}/api/events", server.uri()))
        .with_allow_http(true)
        .with_access_key(TEST_KEY)
        .with_schema(EventGridSchema::CloudEvents)
        .with_format(OutputFormat::Template)
        .with_query("people-query")
        .with_query_template("people-query", template)
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("eventgrid-template-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    core.start().await?;
    tokio::time::sleep(PIPELINE_WARMUP).await;

    let props = PropertyMapBuilder::new().with_string("name", "Ada").build();
    handle
        .send_node_insert("person-1", vec!["Person"], props)
        .await?;
    wait_for_requests(&server, 1, 5000).await;

    let events = all_events(&server).await;
    let insert = events.last().expect("insert event");
    assert_eq!(
        insert["data"]["greeting"], "hello Ada",
        "rendered template data"
    );
    // Metadata flattened as a CloudEvents extension attribute.
    assert_eq!(insert["category"], "people");

    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn eventgrid_publish_failure_keeps_reaction_running() -> Result<()> {
    let server = MockServer::start().await;
    // Topic rejects with 401 (non-retriable).
    Mock::given(method("POST"))
        .and(path("/api/events"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let (source, handle) = make_source("test-source");
    let query = Query::cypher("people-query")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build();
    let reaction = EventGridReaction::builder("eventgrid-fail")
        .with_endpoint(format!("{}/api/events", server.uri()))
        .with_allow_http(true)
        .with_access_key("wrong-key")
        .with_format(OutputFormat::Unpacked)
        .with_query("people-query")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("eventgrid-fail-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    core.start().await?;
    tokio::time::sleep(PIPELINE_WARMUP).await;

    let props = PropertyMapBuilder::new().with_string("name", "Ada").build();
    handle
        .send_node_insert("person-1", vec!["Person"], props)
        .await?;
    wait_for_requests(&server, 1, 5000).await;

    // A second change should still be attempted (the loop survives the failure).
    let props = PropertyMapBuilder::new()
        .with_string("name", "Grace")
        .build();
    handle
        .send_node_insert("person-2", vec!["Person"], props)
        .await?;
    wait_for_requests(&server, 2, 5000).await;

    core.stop().await?;
    Ok(())
}
