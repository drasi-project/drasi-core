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

//! Real source/query/SSE integration on the supported native query runtime.

use anyhow::{bail, Context, Result};
use drasi_lib::{ComponentId, ComponentName, ComponentStatus, DrasiLib, Query};
use drasi_reaction_sse::{QueryConfig, SseExtension, SseReaction, TemplateSpec};
use futures_util::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};
use shared_tests::{
    mock_source::{MockSourceHandle, PropertyMapBuilder},
    recovery_test_helpers::build_core_with_reaction_queries,
};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(10);
const PERSON_QUERY: &str = "MATCH (p:Person) RETURN p.name AS name, p.age AS age";
const COUNT_QUERY: &str = "MATCH (s:Sensor) RETURN count(s) AS sensor_count";
type Events = BoxStream<'static, Result<eventsource_stream::Event>>;

struct Fixture {
    core: Arc<DrasiLib>,
    source: MockSourceHandle,
    client: reqwest::Client,
    base_url: String,
    query_id: ComponentId,
    reaction_id: ComponentId,
}

impl Fixture {
    async fn new(
        test_id: &str,
        queries: &[(&str, &str)],
        routes: Vec<(&str, QueryConfig)>,
    ) -> Result<Self> {
        let source_name = ComponentName::try_new(format!("{test_id}-source"))?;
        let declarations = queries
            .iter()
            .map(|(name, sql)| {
                Query::cypher(ComponentName::try_new(*name)?)
                    .query(*sql)
                    .from_input_by_name(source_name.clone())
                    .with_outbox_capacity(200)
                    .auto_start(true)
                    .build()
            })
            .collect::<Result<Vec<_>>>()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let reaction_id = ComponentId::generate();
        let mut reaction = SseReaction::builder(reaction_id.clone())
            .with_host("127.0.0.1")
            .with_port(port)
            .with_heartbeat_interval_ms(20);
        for (name, _) in queries {
            reaction = reaction.with_query_by_name(ComponentName::try_new(*name)?);
        }
        for (name, config) in routes {
            reaction = reaction.with_route(name, config);
        }
        let (core, source, _) = build_core_with_reaction_queries(
            test_id,
            declarations,
            &ComponentName::try_new("sse")?,
            reaction.build()?,
        )
        .await?;
        let query_id = core
            .component_by_name(&ComponentName::try_new("q1")?)?
            .id()
            .clone();
        drop(listener);
        timeout(DEADLINE, core.start()).await??;
        assert_eq!(
            core.get_reaction_status_by_id(&reaction_id).await?,
            ComponentStatus::Running
        );
        Ok(Self {
            core,
            source,
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(2))
                .build()?,
            base_url: format!("http://127.0.0.1:{port}"),
            query_id,
            reaction_id,
        })
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        Ok(timeout(
            DEADLINE,
            self.client.get(format!("{}{path}", self.base_url)).send(),
        )
        .await??)
    }

    async fn events(&self, path: &str) -> Result<Events> {
        let response = self.get(path).await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        Ok(
            eventsource_stream::EventStream::new(response.bytes_stream())
                .map(|event| event.map_err(anyhow::Error::from))
                .boxed(),
        )
    }

    async fn person(&self, id: &str, name: &str, age: i64) -> Result<()> {
        self.source
            .send_node_insert(
                id,
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", name)
                    .with_integer("age", age)
                    .build(),
            )
            .await
    }

    async fn sensor(&self, id: &str) -> Result<()> {
        self.source
            .send_node_insert(
                id,
                vec!["Sensor"],
                PropertyMapBuilder::new()
                    .with_string("location", id)
                    .with_float("temperature", 22.5)
                    .build(),
            )
            .await
    }

    async fn wait_rows(&self, query_name: &str, count: usize) -> Result<()> {
        let name = ComponentName::try_new(query_name)?;
        timeout(DEADLINE, async {
            loop {
                if self.core.get_query_results_by_name(&name).await?.len() == count {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| format!("waiting for {query_name} to commit {count} rows"))??;
        Ok(())
    }

    async fn snapshot(&self) -> Result<Value> {
        let response = self.get(&format!("/snapshot/{}", self.query_id)).await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        Ok(timeout(DEADLINE, response.json()).await??)
    }

    async fn shutdown(&self) -> Result<()> {
        let reaction = self
            .core
            .reaction_manager()
            .get_reaction_instance_by_id(&self.reaction_id)
            .await?;
        timeout(DEADLINE, self.core.shutdown()).await??;
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        assert!(!self.core.is_running().await);
        Ok(())
    }
}

async fn next_data(events: &mut Events) -> Result<Value> {
    timeout(DEADLINE, async {
        while let Some(event) = events.next().await {
            let payload: Value = serde_json::from_str(&event?.data)?;
            if payload["type"] != "heartbeat" {
                return Ok(payload);
            }
        }
        bail!("SSE stream closed before delivering query data")
    })
    .await
    .context("waiting for an actual non-heartbeat SSE event")?
}

fn aggregation(payload: &Value) -> Result<&Value> {
    payload["results"]
        .as_array()
        .context("default SSE payload needs a results array")?
        .iter()
        .find(|change| change["type"] == "aggregation")
        .context("SSE payload needs an aggregation change")
}

fn assert_people(rows: &Value, expected: &[(&str, i64)]) -> Result<()> {
    let rows = rows.as_array().context("snapshot must be a JSON array")?;
    assert_eq!(rows.len(), expected.len());
    for (name, age) in expected {
        assert!(
            rows.iter()
                .any(|row| row["name"] == *name && row["age"] == *age),
            "Missing expected Person {name}/{age} in {rows:?}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_basic_integration() -> Result<()> {
    let fixture = Fixture::new("sse-basic", &[("q1", PERSON_QUERY)], vec![]).await?;
    let mut events = fixture.events("/events").await?;
    fixture.person("alice", "Alice", 30).await?;
    let payload = next_data(&mut events).await?;
    let changes = payload["results"]
        .as_array()
        .context("query result batch")?;
    assert!(changes
        .iter()
        .any(|change| change["data"] == json!({"name": "Alice", "age": 30})));
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_custom_templates_integration() -> Result<()> {
    let route = QueryConfig {
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
    let fixture =
        Fixture::new("sse-template", &[("q1", PERSON_QUERY)], vec![("q1", route)]).await?;
    let mut events = fixture.events("/events").await?;
    fixture.person("alice", "Alice", 30).await?;
    assert_eq!(
        next_data(&mut events).await?,
        json!({"event": "person_added", "name": "Alice", "age": 30})
    );
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_aggregation_default_format() -> Result<()> {
    let fixture = Fixture::new("sse-count", &[("q1", COUNT_QUERY)], vec![]).await?;
    let mut events = fixture.events("/events").await?;
    fixture.sensor("sensor-1").await?;
    let payload = next_data(&mut events).await?;
    assert_eq!(aggregation(&payload)?["after"]["sensor_count"], 1);
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_aggregation_custom_template() -> Result<()> {
    let route = QueryConfig {
        added: None,
        updated: Some(TemplateSpec::with_extension(
            r#"{"event":"aggregation_update","sensor_count":{{after.sensor_count}}}"#,
            SseExtension { path: None },
        )),
        deleted: None,
    };
    let fixture = Fixture::new(
        "sse-count-template",
        &[("q1", COUNT_QUERY)],
        vec![("q1", route)],
    )
    .await?;
    let mut events = fixture.events("/events").await?;
    fixture.sensor("sensor-1").await?;
    assert_eq!(
        next_data(&mut events).await?,
        json!({"event": "aggregation_update", "sensor_count": 1})
    );
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_aggregation_before_after() -> Result<()> {
    let fixture = Fixture::new("sse-count-before-after", &[("q1", COUNT_QUERY)], vec![]).await?;
    let mut events = fixture.events("/events").await?;
    fixture.sensor("sensor-1").await?;
    let first = next_data(&mut events).await?;
    assert_eq!(aggregation(&first)?["before"]["sensor_count"], 0);
    assert_eq!(aggregation(&first)?["after"]["sensor_count"], 1);
    fixture.sensor("sensor-2").await?;
    let second = next_data(&mut events).await?;
    assert_eq!(aggregation(&second)?["before"]["sensor_count"], 1);
    assert_eq!(aggregation(&second)?["after"]["sensor_count"], 2);
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_multi_path_integration() -> Result<()> {
    let persons = QueryConfig {
        added: Some(TemplateSpec::with_extension(
            r#"{"type":"person","name":"{{after.name}}"}"#,
            SseExtension {
                path: Some("/persons".to_string()),
            },
        )),
        updated: None,
        deleted: None,
    };
    let companies = QueryConfig {
        added: Some(TemplateSpec::with_extension(
            r#"{"type":"company","name":"{{after.name}}"}"#,
            SseExtension {
                path: Some("/companies".to_string()),
            },
        )),
        updated: None,
        deleted: None,
    };
    let fixture = Fixture::new(
        "sse-routes",
        &[
            ("q1", PERSON_QUERY),
            ("companies", "MATCH (c:Company) RETURN c.name AS name"),
        ],
        vec![("q1", persons), ("companies", companies)],
    )
    .await?;
    let mut persons = fixture.events("/persons").await?;
    let mut companies = fixture.events("/companies").await?;
    fixture.person("charlie", "Charlie", 30).await?;
    fixture
        .source
        .send_node_insert(
            "acme",
            vec!["Company"],
            PropertyMapBuilder::new()
                .with_string("name", "Acme")
                .build(),
        )
        .await?;
    assert_eq!(
        next_data(&mut persons).await?,
        json!({"type": "person", "name": "Charlie"})
    );
    assert_eq!(
        next_data(&mut companies).await?,
        json!({"type": "company", "name": "Acme"})
    );
    // Cross-route leakage would appear before the next matching event.
    fixture.person("alice", "Alice", 31).await?;
    fixture
        .source
        .send_node_insert(
            "second-company",
            vec!["Company"],
            PropertyMapBuilder::new()
                .with_string("name", "Second")
                .build(),
        )
        .await?;
    assert_eq!(
        next_data(&mut persons).await?,
        json!({"type": "person", "name": "Alice"})
    );
    assert_eq!(
        next_data(&mut companies).await?,
        json!({"type": "company", "name": "Second"})
    );
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_snapshot_endpoint() -> Result<()> {
    let fixture = Fixture::new("sse-snapshot", &[("q1", PERSON_QUERY)], vec![]).await?;
    let mut events = fixture.events("/events").await?;
    fixture.person("alice", "Alice", 30).await?;
    next_data(&mut events).await?;
    fixture.person("bob", "Bob", 25).await?;
    next_data(&mut events).await?;
    fixture.wait_rows("q1", 2).await?;
    assert_people(&fixture.snapshot().await?, &[("Alice", 30), ("Bob", 25)])?;
    assert_eq!(
        fixture
            .get(&format!("/snapshot/{}", ComponentId::generate()))
            .await?
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_snapshot_empty_result() -> Result<()> {
    let fixture = Fixture::new("sse-empty-snapshot", &[("q1", PERSON_QUERY)], vec![]).await?;
    assert_eq!(fixture.snapshot().await?, json!([]));
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_snapshot_large_result() -> Result<()> {
    let fixture = Fixture::new("sse-large-snapshot", &[("q1", PERSON_QUERY)], vec![]).await?;
    for i in 0..100 {
        fixture
            .person(&format!("p-{i}"), &format!("Person-{i}"), i)
            .await?;
    }
    fixture.wait_rows("q1", 100).await?;
    let response = fixture
        .get(&format!("/snapshot/{}", fixture.query_id))
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["transfer-encoding"], "chunked");
    assert_eq!(response.headers()["content-type"], "application/json");
    assert!(response.headers().get("content-length").is_none());
    let body: Value = timeout(DEADLINE, response.json()).await??;
    let rows = body
        .as_array()
        .context("large snapshot must be a JSON array")?;
    assert_eq!(rows.len(), 100);
    for i in 0..100 {
        assert!(rows
            .iter()
            .any(|row| { row["name"] == format!("Person-{i}") && row["age"] == i }));
    }
    fixture.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn test_sse_snapshot_recovery_after_restart() -> Result<()> {
    let fixture = Fixture::new("sse-snapshot-restart", &[("q1", PERSON_QUERY)], vec![]).await?;
    fixture.person("alice", "Alice", 30).await?;
    fixture.person("bob", "Bob", 25).await?;
    fixture.wait_rows("q1", 2).await?;
    let before = fixture.snapshot().await?;
    assert_people(&before, &[("Alice", 30), ("Bob", 25)])?;
    let query = fixture
        .core
        .query_manager()
        .get_query_instance_by_id(&fixture.query_id)
        .await?;
    let head = query.fetch_snapshot().await?;
    timeout(
        DEADLINE,
        fixture.core.stop_reaction_by_id(&fixture.reaction_id),
    )
    .await??;
    assert_eq!(
        fixture
            .core
            .get_reaction_status_by_id(&fixture.reaction_id)
            .await?,
        ComponentStatus::Stopped
    );
    timeout(
        DEADLINE,
        fixture.core.start_reaction_by_id(&fixture.reaction_id),
    )
    .await??;
    assert_people(&fixture.snapshot().await?, &[("Alice", 30), ("Bob", 25)])?;
    let after = query.fetch_snapshot().await?;
    assert_eq!(after.as_of_sequence, head.as_of_sequence);
    assert_eq!(after.config_hash, head.config_hash);
    assert_eq!(after.output_generation, head.output_generation);
    fixture.shutdown().await
}
