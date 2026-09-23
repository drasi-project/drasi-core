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
