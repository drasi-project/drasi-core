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

//! LifecycleManager and lifecycle-helper behavior retained on the single graph runtime.

use super::*;
use crate::{sources::tests::TestMockSource, test_helpers::wait_for_component_status};

async fn source_core(id: &str, auto_start: bool) -> DrasiLib {
    builder()
        .with_source(TestMockSource::with_auto_start(id.into(), auto_start).unwrap())
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn empty_native_instance_accepts_node_first_additions_without_placeholder() {
    let core = crate::test_helpers::managers::empty_core().await;
    let control = core.computation_control().unwrap();
    assert!(control.desired_snapshot().nodes.is_empty());
    assert!(control.observed().components.is_empty());

    let source = core
        .add_source_with_handle(TestMockSource::new("first-source".into()).unwrap())
        .await
        .unwrap();
    source.wait_created().await.unwrap();
    assert_eq!(control.desired_snapshot().nodes.len(), 1);
    assert_eq!(source.id().as_str(), "first-source");
    core.start().await.unwrap();
    source.wait_started().await.unwrap();
    core.stop().await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn default_builder_supports_empty_add_remove_readd_without_placeholder() {
    let core = DrasiLib::builder().build().await.unwrap();
    let control = core.computation_control().unwrap();
    let desired = control.desired_snapshot();
    assert_eq!(desired.nodes.len(), 1);
    assert_eq!(
        desired.nodes[0].descriptor.id().as_str(),
        crate::sources::COMPONENT_GRAPH_SOURCE_ID
    );
    assert!(core.list_queries().await.unwrap().is_empty());
    assert!(core.list_reactions().await.unwrap().is_empty());
    core.start().await.unwrap();
    core.computation_component(crate::sources::COMPONENT_GRAPH_SOURCE_ID)
        .unwrap()
        .wait_started()
        .await
        .unwrap();

    // Remove the real inspection source too, so controller liveness cannot
    // accidentally depend on any remaining component task.
    core.remove_source(crate::sources::COMPONENT_GRAPH_SOURCE_ID, false)
        .await
        .unwrap();
    assert!(control.desired_snapshot().nodes.is_empty());
    assert!(control.observed().components.is_empty());

    let first = core
        .add_source_with_handle(TestMockSource::new("user-source".into()).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), first.wait_started())
        .await
        .expect("empty controller must accept and activate its first addition")
        .unwrap();
    core.remove_source("user-source", false).await.unwrap();
    assert!(control.desired_snapshot().nodes.is_empty());
    assert!(control.observed().components.is_empty());
    assert!(first.observed().is_err());

    core.stop().await.unwrap();
    core.start().await.unwrap();
    let replacement = core
        .add_source_with_handle(TestMockSource::new("user-source".into()).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), replacement.wait_started())
        .await
        .expect("controller must stay available after its last component is removed")
        .unwrap();
    assert_eq!(
        core.get_source_status("user-source").await.unwrap(),
        ComponentStatus::Running
    );
    assert!(
        first.observed().is_err(),
        "retired generation must not revive"
    );
    core.remove_source("user-source", false).await.unwrap();
    assert!(control.desired_snapshot().nodes.is_empty());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn clean_restart_preserves_query_generation_and_strict_reaction_checkpoint() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = Arc::new(crate::MemoryStateStoreProvider::new());
        let (mut reaction, _, mut output) = ControlledReaction::new("reaction", &["query"]);
        reaction.base = ReactionBase::new(
            ReactionBaseParams::new("reaction", vec!["query".into()]).with_auto_start(true),
        );
        let (source, source_control) = ControlledSource::new("source");
        source_control.auto_start.store(true, Ordering::Release);
        let core = DrasiLib::builder()
            .with_state_store_provider(store.clone())
            .with_source(source)
            .with_query(
                crate::Query::cypher("query")
                    .query("MATCH (n:Person) RETURN n.name AS name")
                    .from_source("source")
                    .build(),
            )
            .with_reaction(reaction)
            .build()
            .await
            .unwrap();
        core.start().await.unwrap();
        for (sequence, name) in [(1, "Alice"), (2, "Bob")] {
            insert_person(&core, "source", &sequence.to_string(), name).await;
            assert_eq!(output.recv().await.unwrap().sequence, sequence);
        }
        let query = core
            .query_manager()
            .get_query_instance("query")
            .await
            .unwrap();
        let before = query.fetch_snapshot().await.unwrap();
        let generation = query.output_generation().await;
        let checkpoint = crate::ReactionCheckpoint {
            sequence: before.as_of_sequence,
            config_hash: before.config_hash,
        };
        // The application has consumed both results; enqueue alone is not completion.
        crate::reactions::checkpoint::write_checkpoint(
            store.as_ref(),
            "reaction",
            "query",
            &checkpoint,
        )
        .await
        .unwrap();
        let handle = core.computation_component("query").unwrap();
        let node_generation = handle.generation();
        let control = core.computation_control().unwrap();
        control
            .quiesce_components(
                control.desired_snapshot().revision,
                GraphSelection::Exact(vec![ComponentId::try_new("query").unwrap()]),
            )
            .await
            .unwrap();
        handle.start().await.unwrap();
        assert_eq!(query.output_generation().await, generation);
        assert_eq!(handle.generation(), node_generation);

        core.stop().await.unwrap();
        core.start().await.unwrap();
        core.computation_component("reaction")
            .unwrap()
            .wait_started()
            .await
            .unwrap();
        let after = query.fetch_snapshot().await.unwrap();
        assert_eq!(query.output_generation().await, generation);
        assert_eq!(after.config_hash, before.config_hash);
        assert_eq!(after.as_of_sequence, 2);
        assert_eq!(after.len(), 2);
        assert_eq!(
            core.computation_component("query").unwrap().generation(),
            node_generation
        );
        assert_eq!(
            crate::reactions::checkpoint::read_checkpoint(store.as_ref(), "reaction", "query")
                .await
                .unwrap(),
            Some(checkpoint),
        );
        insert_person(&core, "source", "3", "Carol").await;
        let result = output.recv().await.unwrap();
        assert_eq!(result.sequence, 3);
        assert!(
            matches!(&result.results[..], [crate::channels::ResultDiff::Add { data, .. }]
            if data == &serde_json::json!({"name": "Carol"}))
        );
        assert_eq!(source_control.initializations.load(Ordering::Acquire), 1);
        assert_eq!(source_control.starts.load(Ordering::Acquire), 2);
        core.shutdown().await.unwrap();
    })
    .await
    .expect("clean restart must preserve state and keep Strict reaction recovery available");
}

#[tokio::test]
async fn start_components_starts_auto_start_sources() {
    let core = source_core("auto-src", true).await;
    let mut events = core.subscribe_all_component_events();
    core.start().await.unwrap();
    wait_for_component_status(
        &mut events,
        "auto-src",
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        core.get_source_status("auto-src").await.unwrap(),
        ComponentStatus::Running
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn start_components_skips_non_auto_start() {
    let core = source_core("manual-src", false).await;
    core.start().await.unwrap();
    assert_eq!(
        core.get_source_status("manual-src").await.unwrap(),
        ComponentStatus::Added
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_all_components_stops_running() {
    let core = source_core("stop-src", true).await;
    let mut events = core.subscribe_all_component_events();
    core.start().await.unwrap();
    wait_for_component_status(
        &mut events,
        "stop-src",
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await;
    core.stop().await.unwrap();
    wait_for_component_status(
        &mut events,
        "stop-src",
        ComponentStatus::Stopped,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        core.get_source_status("stop-src").await.unwrap(),
        ComponentStatus::Stopped
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_all_components_handles_already_stopped() {
    let core = source_core("idle-src", false).await;
    core.start().await.unwrap();
    core.stop().await.unwrap();
    assert_eq!(
        core.get_source_status("idle-src").await.unwrap(),
        ComponentStatus::Added
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn load_configuration_creates_queries_from_config() {
    let core = builder()
        .with_source(TestMockSource::with_auto_start("cfg-src".into(), true).unwrap())
        .with_query(config("cfg-query", Some("cfg-src")))
        .build()
        .await
        .unwrap();
    assert_eq!(
        core.list_queries().await.unwrap(),
        vec![("cfg-query".into(), ComponentStatus::Added)]
    );
    core.computation_component("cfg-query")
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_start_component_success() {
    let core = source_core("source", false).await;
    let mut events = core.subscribe_all_component_events();
    core.start_source("source").await.unwrap();
    wait_for_component_status(
        &mut events,
        "source",
        ComponentStatus::Starting,
        Duration::from_secs(5),
    )
    .await;
    core.computation_component("source")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Running
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_start_component_failure_reverts_to_error() {
    let (source, control) = ControlledSource::new("source");
    control.fail_starts.store(1, Ordering::Release);
    let core = builder().with_source(source).build().await.unwrap();
    assert!(core.start_source("source").await.is_err());
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Error
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_stop_component_success() {
    let core = source_core("source", false).await;
    core.start_source("source").await.unwrap();
    core.computation_component("source")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let mut events = core.subscribe_all_component_events();
    core.stop_source("source").await.unwrap();
    wait_for_component_status(
        &mut events,
        "source",
        ComponentStatus::Stopping,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Stopped
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_stop_component_failure_reverts_to_error() {
    let (source, control) = ControlledSource::new("source");
    let core = builder().with_source(source).build().await.unwrap();
    core.start_source("source").await.unwrap();
    core.computation_component("source")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    control.fail_stop.store(true, Ordering::Release);
    assert!(core.stop_source("source").await.is_err());
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Error
    );
    control.fail_stop.store(false, Ordering::Release);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_get_component_status_found() {
    let core = source_core("source", false).await;
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Added
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_get_component_status_not_found() {
    let core = builder().build().await.unwrap();
    assert!(matches!(
        core.get_source_status("missing").await,
        Err(crate::DrasiError::ComponentNotFound { .. })
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_list_components() {
    let core = builder()
        .with_source(TestMockSource::new("s1".into()).unwrap())
        .with_source(TestMockSource::new("s2".into()).unwrap())
        .build()
        .await
        .unwrap();
    let sources = core.list_sources().await.unwrap();
    assert_eq!(
        sources
            .iter()
            .filter(|(id, _)| id != crate::sources::COMPONENT_GRAPH_SOURCE_ID)
            .count(),
        2
    );
    core.shutdown().await.unwrap();
}
