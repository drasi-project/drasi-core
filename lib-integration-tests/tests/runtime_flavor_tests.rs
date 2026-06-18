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

use drasi_lib::channels::ResultDiff;
use drasi_lib::{ComponentStatus, DrasiLib, Query};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test(flavor = "current_thread")]
async fn test_drasi_lib_builds_on_current_thread_runtime() {
    let drasi = DrasiLib::builder()
        .with_id("current-thread-runtime")
        .build()
        .await
        .expect("Failed to build DrasiLib on current_thread runtime");

    drasi.stop().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_drasi_lib_builds_on_multi_thread_runtime() {
    let drasi = DrasiLib::builder()
        .with_id("multi-thread-runtime")
        .build()
        .await
        .expect("Failed to build DrasiLib on multi_thread runtime");

    drasi.stop().await.ok();
}

#[tokio::test(flavor = "current_thread")]
async fn test_spawn_blocking_works_on_current_thread_runtime() {
    let result = tokio::task::spawn_blocking(|| 21 * 2)
        .await
        .expect("spawn_blocking task panicked on current_thread runtime");

    assert_eq!(result, 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_spawn_blocking_works_on_multi_thread_runtime() {
    let result = tokio::task::spawn_blocking(|| 21 * 2)
        .await
        .expect("spawn_blocking task panicked on multi_thread runtime");

    assert_eq!(result, 42);
}

/// Wait for a component to reach Running status via event notification.
async fn wait_for_running(drasi: &DrasiLib, component_id: &str) {
    let mut rx = drasi.subscribe_all_component_events();
    let snapshot = drasi.get_graph().await;
    if snapshot
        .nodes
        .iter()
        .any(|n| n.id == component_id && n.status == ComponentStatus::Running)
    {
        return;
    }
    timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event)
                    if event.component_id == component_id
                        && event.status == ComponentStatus::Running =>
                {
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("Event channel closed while waiting for '{component_id}'");
                }
                _ => continue,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Timed out waiting for '{component_id}' to reach Running"));
}

/// Drive a full source -> query -> diff cycle and assert the result is delivered.
///
/// This exercises source dispatch, continuous query evaluation, and reaction
/// result delivery under whichever Tokio runtime flavor the test runs on,
/// validating that the library functions correctly (and not just initializes)
/// on a single-threaded runtime.
async fn run_source_query_diff_cycle(instance_id: &str) {
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: None,
    };
    let (app_source, app_handle) =
        ApplicationSource::new("flavor-source", config).expect("create application source");

    let (app_reaction, reaction_handle) = ApplicationReactionBuilder::new("flavor-reaction")
        .with_query("flavor-query")
        .with_auto_start(true)
        .build();

    let query = Query::cypher("flavor-query")
        .query("MATCH (i:Item) RETURN i.name AS name")
        .from_source("flavor-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id(instance_id)
            .with_source(app_source)
            .with_query(query)
            .with_reaction(app_reaction)
            .build()
            .await
            .expect("build DrasiLib"),
    );

    drasi.start().await.expect("start DrasiLib");

    wait_for_running(&drasi, "flavor-source").await;
    wait_for_running(&drasi, "flavor-query").await;
    wait_for_running(&drasi, "flavor-reaction").await;

    let mut subscription = reaction_handle
        .subscribe_with_options(Default::default())
        .await
        .expect("subscribe to reaction");

    app_handle
        .send_node_insert(
            "item-1",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("name", "widget")
                .build(),
        )
        .await
        .expect("send node insert");

    let result = timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("timed out waiting for query result")
        .expect("reaction subscription closed without a result");

    assert_eq!(result.query_id, "flavor-query");
    assert!(
        !result.results.is_empty(),
        "expected at least one result row from the query"
    );

    let data = match &result.results[0] {
        ResultDiff::Add { data, .. } => data,
        other => panic!("expected an Add result diff, got {other:?}"),
    };
    assert_eq!(
        data.get("name").and_then(|v| v.as_str()),
        Some("widget"),
        "query result did not contain the injected node value"
    );

    drasi.stop().await.ok();
}

#[tokio::test(flavor = "current_thread")]
async fn test_source_query_diff_cycle_on_current_thread_runtime() {
    run_source_query_diff_cycle("flavor-e2e-current-thread").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_source_query_diff_cycle_on_multi_thread_runtime() {
    run_source_query_diff_cycle("flavor-e2e-multi-thread").await;
}
