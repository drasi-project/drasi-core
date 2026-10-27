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
use drasi_lib::computation::v1::{ComponentId, ComponentLifecycle};
use drasi_lib::{ComponentStatus, DrasiLib, Query};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::collections::{BTreeSet, HashMap};
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
        .from_input("flavor-source")
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

async fn stop_query_helpers_after_native_stop(flavor: &str) -> anyhow::Result<()> {
    for operation in ["query", "query-instance", "all-queries", "instance"] {
        let (source, source_handle) = ApplicationSource::new(
            "source",
            ApplicationSourceConfig {
                properties: HashMap::new(),
                durability: None,
            },
        )?;
        let query = |id| {
            Query::cypher(id)
                .query("MATCH (i:Item) RETURN i.name AS name")
                .from_input("source")
                .auto_start(true)
                .enable_bootstrap(false)
                .build()
        };
        let drasi = DrasiLib::builder()
            .with_id(format!("query-stop-{flavor}-{operation}"))
            .with_source(source)
            .with_query(query("query"))
            .with_query(query("sibling"))
            .build()
            .await?;
        drasi.start().await?;
        drasi
            .computation_component("sibling")?
            .wait_started()
            .await?;
        let control = drasi.computation_control()?;
        let id = ComponentId::try_new("query")?;
        let desired = control.desired_snapshot();
        let members: BTreeSet<_> = std::iter::once(id.clone())
            .chain(desired.edges.iter().filter_map(|edge| {
                if edge.definition.to.component == id {
                    Some(edge.definition.from.component.clone())
                } else if edge.definition.from.component == id {
                    Some(edge.definition.to.component.clone())
                } else {
                    None
                }
            }))
            .collect();
        assert!(members.len() > 1);
        for member in &members {
            control.component_handle(member)?.wait_started().await?;
        }

        // Native control can stop the evaluator without stopping its ordinary-API helpers.
        control.component_handle(&id)?.stop().await?;
        let stopped = control.observed();
        assert_eq!(
            stopped.components[&id].lifecycle,
            ComponentLifecycle::Stopped
        );
        assert!(members
            .iter()
            .any(|member| { stopped.components[member].lifecycle == ComponentLifecycle::Running }));

        match operation {
            "query" => drasi.stop_query("query").await?,
            "query-instance" => {
                drasi
                    .query_manager()
                    .get_query_instance("query")
                    .await
                    .map_err(anyhow::Error::msg)?
                    .stop()
                    .await?;
            }
            "all-queries" => drasi.query_manager().stop_all().await?,
            "instance" => drasi.stop().await?,
            _ => unreachable!(),
        }
        let stopped = control.observed();
        for member in &members {
            assert_eq!(
                stopped.components[member].lifecycle,
                ComponentLifecycle::Stopped,
                "{operation} left query helper '{member}' active"
            );
        }
        if matches!(operation, "query" | "query-instance") {
            assert_eq!(
                drasi.get_query_status("sibling").await?,
                ComponentStatus::Running
            );
            let mut results = drasi
                .query_manager()
                .get_query_instance("sibling")
                .await
                .map_err(anyhow::Error::msg)?
                .subscribe("sibling-progress".into())
                .await?
                .receiver;
            source_handle
                .send_node_insert(
                    "item",
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_string("name", "widget")
                        .build(),
                )
                .await?;
            let result = timeout(Duration::from_secs(5), results.recv()).await??;
            assert_eq!(result.query_id, "sibling");
            assert!(matches!(&result.results[..], [ResultDiff::Add { data, .. }]
                if data == &serde_json::json!({"name": "widget"})));
        }
        if operation != "instance" {
            assert_eq!(
                drasi.get_source_status("source").await?,
                ComponentStatus::Running
            );
        }
        if operation == "query" {
            let error = drasi
                .stop_query("query")
                .await
                .expect_err("a fully stopped query must retain the already-stopped error");
            assert!(format!("{error:#}").contains("already stopped"));
        } else if operation == "query-instance" {
            drasi
                .query_manager()
                .get_query_instance("query")
                .await
                .map_err(anyhow::Error::msg)?
                .stop()
                .await?;
        }
        drasi.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_query_stop_cleans_up_helpers_on_current_thread_runtime() {
    timeout(
        Duration::from_secs(20),
        stop_query_helpers_after_native_stop("current-thread"),
    )
    .await
    .expect("query helper cleanup must complete")
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_query_stop_cleans_up_helpers_on_multi_thread_runtime() {
    timeout(
        Duration::from_secs(20),
        stop_query_helpers_after_native_stop("multi-thread"),
    )
    .await
    .expect("query helper cleanup must complete")
    .unwrap();
}
