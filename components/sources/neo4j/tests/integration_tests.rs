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

mod neo4j_helpers;

use anyhow::Result;
use drasi_bootstrap_neo4j::{Neo4jBootstrapConfig, Neo4jBootstrapProvider};
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_neo4j::Neo4jSource;
use neo4j_helpers::{reset_graph, setup_neo4j};
use neo4rs::query;
use serial_test::serial;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

async fn wait_for_query_results(
    core: &Arc<DrasiLib>,
    query_id: &str,
    predicate: impl Fn(&[serde_json::Value]) -> bool,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        let results = core.get_query_results(query_id).await?;
        if predicate(&results) {
            return Ok(());
        }
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out waiting for query results for query_id `{query_id}`. Latest results: {results:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn build_person_core(
    config: &neo4j_helpers::Neo4jConfig,
    query_id: &str,
) -> Result<Arc<DrasiLib>> {
    let bootstrap_provider = Neo4jBootstrapProvider::new(Neo4jBootstrapConfig {
        uri: format!("bolt://{}", config.bolt_uri()),
        user: config.user.clone(),
        password: config.password.clone(),
        database: config.database.clone(),
        labels: vec!["Person".to_string(), "Movie".to_string()],
        rel_types: vec!["ACTED_IN".to_string()],
    });

    let source = Neo4jSource::builder("neo4j-test-source")
        .with_uri(format!("bolt://{}", config.bolt_uri()))
        .with_user(config.user.clone())
        .with_password(config.password.clone())
        .with_database(config.database.clone())
        .with_labels(vec!["Person".to_string(), "Movie".to_string()])
        .with_rel_types(vec!["ACTED_IN".to_string()])
        .with_poll_interval(Duration::from_millis(200))
        .with_bootstrap_provider(bootstrap_provider)
        .start_from_now()
        .build()?;

    let query = Query::cypher(query_id)
        .query(
            r#"
            MATCH (p:Person)
            RETURN p.id AS id, p.name AS name
            "#,
        )
        .from_source("neo4j-test-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, _handle) = ApplicationReaction::builder("test-reaction")
        .with_query(query_id)
        .build();

    Ok(Arc::new(
        DrasiLib::builder()
            .with_id("neo4j-test-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    ))
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_source_connects_and_starts() -> Result<()> {
    init_logging();
    let neo = setup_neo4j().await?;
    let graph = neo.get_graph().await?;
    reset_graph(&graph).await?;

    let core = build_person_core(neo.config(), "status-query").await?;
    core.start().await?;

    wait_for_query_results(&core, "status-query", |results| results.is_empty()).await?;
    let status = core.get_source_status("neo4j-test-source").await?;
    assert_eq!(status, drasi_lib::channels::ComponentStatus::Running);

    core.stop().await?;
    neo.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_insert_update_delete_detection() -> Result<()> {
    init_logging();
    let neo = setup_neo4j().await?;
    let graph = neo.get_graph().await?;
    reset_graph(&graph).await?;

    let core = build_person_core(neo.config(), "crud-query").await?;
    core.start().await?;
    wait_for_query_results(&core, "crud-query", |results| results.is_empty()).await?;

    graph
        .run(query("CREATE (:Person {id:'1', name:'Alice', age:30})"))
        .await?;
    wait_for_query_results(&core, "crud-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice".into()))
    })
    .await?;

    graph
        .run(query("MATCH (p:Person {id:'1'}) SET p.name = 'Bob'"))
        .await?;
    wait_for_query_results(&core, "crud-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Bob".into()))
    })
    .await?;

    graph
        .run(query("MATCH (p:Person {id:'1'}) DETACH DELETE p"))
        .await?;
    wait_for_query_results(&core, "crud-query", |results| results.is_empty()).await?;

    core.stop().await?;
    neo.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_relationship_detection() -> Result<()> {
    init_logging();
    let neo = setup_neo4j().await?;
    let graph = neo.get_graph().await?;
    reset_graph(&graph).await?;

    graph
        .run(query("CREATE (:Person {id:'1', name:'Keanu'})"))
        .await?;
    graph
        .run(query("CREATE (:Movie {id:'2', title:'The Matrix'})"))
        .await?;
    graph
        .run(query(
            "MATCH (p:Person {id:'1'}), (m:Movie {id:'2'}) CREATE (p)-[:ACTED_IN {role:'Neo'}]->(m)",
        ))
        .await?;

    let provider = Neo4jBootstrapProvider::new(Neo4jBootstrapConfig {
        uri: format!("bolt://{}", neo.config().bolt_uri()),
        user: neo.config().user.clone(),
        password: neo.config().password.clone(),
        database: neo.config().database.clone(),
        labels: vec!["Person".to_string(), "Movie".to_string()],
        rel_types: vec!["ACTED_IN".to_string()],
    });
    let request = BootstrapRequest {
        query_id: "relationship-query".to_string(),
        node_labels: vec!["Person".to_string(), "Movie".to_string()],
        relation_labels: vec!["ACTED_IN".to_string()],
        request_id: "relationship-bootstrap".to_string(),
    };
    let context =
        BootstrapContext::new_minimal("test-server".to_string(), "neo4j-rel-source".to_string());
    let (tx, mut rx) = mpsc::channel(64);
    let emitted = provider.bootstrap(request, &context, tx, None).await?;
    assert!(emitted >= 3);

    let mut saw_relation = false;
    for _ in 0..emitted {
        if let Some(event) = rx.recv().await {
            if let SourceChange::Insert {
                element:
                    Element::Relation {
                        metadata,
                        properties,
                        in_node,
                        out_node,
                    },
            } = event.change
            {
                if metadata
                    .labels
                    .iter()
                    .any(|label| label.as_ref() == "ACTED_IN")
                {
                    let role = properties.get("role").and_then(|value| match value {
                        ElementValue::String(v) => Some(v.as_ref()),
                        _ => None,
                    });
                    assert_eq!(role, Some("Neo"));
                    assert!(!in_node.element_id.is_empty());
                    assert!(!out_node.element_id.is_empty());
                    saw_relation = true;
                }
            }
        }
    }
    assert!(
        saw_relation,
        "Expected ACTED_IN relationship in bootstrap output"
    );

    neo.cleanup().await;
    Ok(())
}
