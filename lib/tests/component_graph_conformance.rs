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

//! Public inspection and plugin status contracts retained after the mutable
//! ComponentGraph implementation was removed.

#[allow(dead_code)]
mod mock_source;

use drasi_lib::{
    channels::{ComponentStatusHandle, ComponentUpdate},
    component_graph::{ComponentKind, RelationshipKind},
    ComponentStatus, DrasiLib, Query,
};
use std::time::Duration;
use tokio::{sync::mpsc, time::timeout};

#[tokio::test]
async fn status_handle_defaults_to_stopped_and_clones_share_updates() {
    let handle = ComponentStatusHandle::new("source");
    let clone = handle.clone();
    assert_eq!(handle.get_status().await, ComponentStatus::Stopped);
    assert_eq!(clone.get_status().await, ComponentStatus::Stopped);
    let mut watch = clone.subscribe_status();
    handle.set_status(ComponentStatus::Running, None).await;
    watch.changed().await.unwrap();
    assert_eq!(*watch.borrow_and_update(), ComponentStatus::Running);
    assert_eq!(clone.get_status().await, ComponentStatus::Running);
    clone.set_status(ComponentStatus::Stopped, None).await;
    assert_eq!(handle.get_status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn wired_status_handle_preserves_id_status_and_error_message() {
    let (sender, mut receiver) = mpsc::channel(1);
    let handle = ComponentStatusHandle::new_wired("source", sender);
    handle
        .set_status(ComponentStatus::Error, Some("injected failure".into()))
        .await;
    assert!(matches!(
        receiver.recv().await,
        Some(ComponentUpdate::Status { component_id, status: ComponentStatus::Error, message })
            if component_id == "source" && message.as_deref() == Some("injected failure")
    ));
    assert_eq!(handle.get_status().await, ComponentStatus::Error);
}

#[tokio::test]
async fn unwired_status_changes_are_local_and_late_wiring_only_takes_effect_once() {
    let handle = ComponentStatusHandle::new("source");
    handle.set_status(ComponentStatus::Starting, None).await;
    assert_eq!(handle.get_status().await, ComponentStatus::Starting);
    let (first, mut first_receiver) = mpsc::channel(2);
    let (second, mut second_receiver) = mpsc::channel(2);
    handle.wire(first).await;
    handle.clone().wire(second).await;
    assert!(first_receiver.try_recv().is_err());
    handle.set_status(ComponentStatus::Running, None).await;
    assert!(matches!(
        first_receiver.recv().await,
        Some(ComponentUpdate::Status {
            status: ComponentStatus::Running,
            ..
        })
    ));
    assert!(second_receiver.try_recv().is_err());
}

#[tokio::test]
async fn plugin_status_reporting_waits_for_capacity_without_losing_updates() {
    let (sender, mut receiver) = mpsc::channel(1);
    let handle = ComponentStatusHandle::new_wired("source", sender);
    handle.set_status(ComponentStatus::Starting, None).await;
    let next = handle.set_status(ComponentStatus::Running, None);
    tokio::pin!(next);
    assert!(futures::poll!(&mut next).is_pending());
    assert!(matches!(
        receiver.recv().await,
        Some(ComponentUpdate::Status {
            status: ComponentStatus::Starting,
            ..
        })
    ));
    next.await;
    assert!(matches!(
        receiver.recv().await,
        Some(ComponentUpdate::Status {
            status: ComponentStatus::Running,
            ..
        })
    ));
}

#[tokio::test]
async fn graph_snapshots_and_events_follow_the_controller_without_a_mutable_shadow() {
    timeout(Duration::from_secs(10), async {
        let (source, _input) = mock_source::MockSource::new("source").unwrap();
        let core = DrasiLib::builder()
            .with_id("inspection-contract")
            .with_source(source)
            .with_query(
                Query::cypher("query")
                    .query("MATCH (n:Item) RETURN n.name AS name")
                    .from_source("source")
                    .enable_bootstrap(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();
        let view = core.component_graph();
        let clone = view.clone();
        let mut events = view.subscribe().unwrap();
        let mut other_events = clone.subscribe().unwrap();
        let before = view.snapshot().await.unwrap();
        assert_eq!(before.instance_id, "inspection-contract");
        assert_eq!(
            before.get_component("inspection-contract").unwrap().kind,
            ComponentKind::Instance
        );
        assert!(!before.contains("absent"));
        assert!(before.get_dependencies("absent").is_empty());
        assert!(before.get_dependents("absent").is_empty());
        assert!(view.get_events("absent").await.unwrap().is_empty());
        let query = core.computation_component("query").unwrap();
        query.wait_created().await.unwrap();
        core.start().await.unwrap();
        query.wait_started().await.unwrap();
        for receiver in [&mut events, &mut other_events] {
            loop {
                let event = receiver.recv().await.unwrap();
                if event.component_id == "query" && event.status == ComponentStatus::Running {
                    break;
                }
            }
        }
        let running = clone.snapshot().await.unwrap();
        assert_eq!(
            running.get_component("query").unwrap().status,
            ComponentStatus::Running
        );
        assert_ne!(
            before.get_component("query").unwrap().status,
            ComponentStatus::Running
        );
        assert_eq!(
            running
                .get_dependencies("query")
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["source"]
        );
        assert_eq!(
            running
                .get_dependents("source")
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["query"]
        );
        assert_eq!(
            running
                .get_neighbors("source", &RelationshipKind::Feeds)
                .len(),
            1
        );
        let serialized = serde_json::to_value(&running).unwrap();
        assert_eq!(serialized["instance_id"], "inspection-contract");
        assert!(view
            .get_events("query")
            .await
            .unwrap()
            .iter()
            .any(|event| { event.status == ComponentStatus::Running }));
        assert!(!view.get_all_events().await.unwrap().is_empty());
        assert!(core.remove_source("source", false).await.is_err());
        assert!(view.snapshot().await.unwrap().contains("source"));
        core.remove_query("query").await.unwrap();
        assert!(!view.snapshot().await.unwrap().contains("query"));
        assert!(core
            .query_manager()
            .get_query_instance("query")
            .await
            .is_err());
        assert!(view
            .snapshot()
            .await
            .unwrap()
            .get_dependents("source")
            .is_empty());
        core.remove_source("source", false).await.unwrap();
        assert!(!view.snapshot().await.unwrap().contains("source"));
        assert!(core.remove_source("source", false).await.is_err());
        core.shutdown().await.unwrap();
    })
    .await
    .expect("inspection contract timed out");
}

#[tokio::test]
async fn read_only_graph_handles_do_not_keep_the_runtime_alive() {
    let core = DrasiLib::builder().build().await.unwrap();
    let view = core.component_graph();
    let clone = view.clone();
    core.shutdown().await.unwrap();
    drop(core);
    assert!(view.snapshot().await.is_err());
    assert!(clone.get_events("source").await.is_err());
    assert!(view.get_all_events().await.is_err());
    assert!(clone.subscribe().is_err());
    assert!(view.inspector().is_err());
}

#[tokio::test]
async fn full_pipeline_preserves_main_ownership_subscription_and_removal_results() {
    use std::collections::BTreeSet;

    timeout(Duration::from_secs(10), async {
        let (source1, _input1) = mock_source::MockSource::new("source-1").unwrap();
        let (source2, _input2) = mock_source::MockSource::new("source-2").unwrap();
        let (reaction, _output) = drasi_reaction_application::ApplicationReaction::new(
            "reaction-1",
            vec!["query-1".into(), "query-2".into()],
        );
        let core = DrasiLib::builder()
            .with_id("test-instance")
            .with_source(source1)
            .with_source(source2)
            .with_query(
                Query::cypher("query-1")
                    .query("MATCH (n:Item) RETURN n.name AS name")
                    .from_source("source-1")
                    .from_source("source-2")
                    .enable_bootstrap(false)
                    .build(),
            )
            .with_query(
                Query::cypher("query-2")
                    .query("MATCH (n:Item) RETURN n")
                    .from_source("source-1")
                    .enable_bootstrap(false)
                    .build(),
            )
            .with_reaction(reaction)
            .build()
            .await
            .unwrap();
        core.remove_source(drasi_lib::sources::COMPONENT_GRAPH_SOURCE_ID, false)
            .await
            .unwrap();
        for id in ["source-1", "source-2", "query-1", "query-2", "reaction-1"] {
            core.computation_component(id)
                .unwrap()
                .wait_created()
                .await
                .unwrap();
        }
        let view = core.component_graph();
        let snapshot = view.snapshot().await.unwrap();
        assert_eq!(snapshot.instance_id, "test-instance");
        assert_eq!(snapshot.nodes.len(), 6);
        let kinds: BTreeSet<_> = snapshot
            .nodes
            .iter()
            .map(|node| format!("{}:{:?}", node.id, node.kind))
            .collect();
        assert_eq!(
            kinds,
            BTreeSet::from([
                "test-instance:Instance".into(),
                "source-1:Source".into(),
                "source-2:Source".into(),
                "query-1:Query".into(),
                "query-2:Query".into(),
                "reaction-1:Reaction".into(),
            ])
        );
        let edges: BTreeSet<_> = snapshot
            .edges
            .iter()
            .map(|edge| format!("{}:{:?}:{}", edge.from, edge.relationship, edge.to))
            .collect();
        let mut expected = BTreeSet::new();
        for id in ["source-1", "source-2", "query-1", "query-2", "reaction-1"] {
            expected.insert(format!("test-instance:Owns:{id}"));
            expected.insert(format!("{id}:OwnedBy:test-instance"));
            assert_eq!(
                snapshot.get_component(id).unwrap().status,
                ComponentStatus::Added
            );
        }
        for (source, consumer) in [
            ("source-1", "query-1"),
            ("source-2", "query-1"),
            ("source-1", "query-2"),
            ("query-1", "reaction-1"),
            ("query-2", "reaction-1"),
        ] {
            expected.insert(format!("{source}:Feeds:{consumer}"));
            expected.insert(format!("{consumer}:SubscribesTo:{source}"));
        }
        assert_eq!(snapshot.edges.len(), 20, "no duplicate edges");
        assert_eq!(edges, expected);
        let serialized = serde_json::to_value(&snapshot).unwrap();
        let roundtrip: drasi_lib::component_graph::GraphSnapshot =
            serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(serde_json::to_value(roundtrip).unwrap(), serialized);
        for id in ["source-1", "source-2"] {
            assert_eq!(snapshot.get_component(id).unwrap().metadata["kind"], "mock");
        }
        assert_eq!(
            snapshot.get_component("query-1").unwrap().metadata["query"],
            "MATCH (n:Item) RETURN n.name AS name"
        );

        assert!(core.remove_source("test-instance", false).await.is_err());
        assert!(core.remove_query("test-instance").await.is_err());
        assert!(core.remove_reaction("test-instance", false).await.is_err());
        assert!(core.remove_source("source-1", false).await.is_err());
        assert!(core.remove_query("query-1").await.is_err());
        assert_eq!(
            serde_json::to_value(view.snapshot().await.unwrap()).unwrap(),
            serialized,
            "rejected removals cannot change membership, status, or relationships"
        );
        core.remove_reaction("reaction-1", false).await.unwrap();
        for id in ["query-1", "query-2"] {
            core.remove_query(id).await.unwrap();
        }
        for id in ["source-1", "source-2"] {
            core.remove_source(id, false).await.unwrap();
        }
        let empty = view.snapshot().await.unwrap();
        assert_eq!(empty.nodes.len(), 1);
        assert_eq!(empty.nodes[0].id, "test-instance");
        assert!(empty.edges.is_empty());
        assert_eq!(snapshot.nodes.len(), 6, "old snapshots remain detached");
        assert_eq!(snapshot.edges.len(), 20);
        core.shutdown().await.unwrap();
    })
    .await
    .expect("main snapshot contract timed out");
}
