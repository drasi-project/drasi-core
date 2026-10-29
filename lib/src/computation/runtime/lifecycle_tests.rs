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

async fn assert_instance_snapshot_status(
    core: &DrasiLib,
    view: &crate::component_graph::ComponentGraphView,
    expected: ComponentStatus,
) {
    for snapshot in [core.get_graph().await.unwrap(), view.snapshot().await.unwrap()] {
        assert_eq!(
            snapshot
                .get_component(&snapshot.instance_id)
                .unwrap()
                .status,
            expected
        );
    }
}

#[tokio::test]
async fn compatibility_instance_snapshot_tracks_start_stop_and_restart() {
    let core = builder().build().await.unwrap();
    let view = core.component_graph();
    assert_instance_snapshot_status(&core, &view, ComponentStatus::Stopped).await;
    for _ in 0..2 {
        core.start().await.unwrap();
        assert_instance_snapshot_status(&core, &view, ComponentStatus::Running).await;
        core.stop().await.unwrap();
        assert_instance_snapshot_status(&core, &view, ComponentStatus::Stopped).await;
    }
    core.shutdown().await.unwrap();
    drop(core);
    assert!(
        view.snapshot().await.is_err(),
        "the view must not retain its owner"
    );
}

#[tokio::test(start_paused = true)]
async fn instance_activation_deadline_is_configurable_and_round_trips() {
    for seconds in [0, u64::MAX] {
        assert!(builder()
            .with_activation_timeout_secs(seconds)
            .build()
            .await
            .is_err());
    }
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control.pending_start.store(true, Ordering::Release);
    let core = builder()
        .with_source(source)
        .with_activation_timeout_secs(2)
        .build()
        .await
        .unwrap();
    assert_eq!(
        core.get_current_config()
            .await
            .unwrap()
            .activation_timeout_secs,
        Some(2)
    );
    let began = tokio::time::Instant::now();
    let error = core
        .start()
        .await
        .expect_err("the held source must reach its activation deadline");
    assert!(error
        .downcast_ref::<crate::error::PartialStartError>()
        .is_some());
    assert!(format!("{error:#}").contains("activation timeout"));
    assert_eq!(began.elapsed(), Duration::from_secs(2));
    core.shutdown().await.unwrap();
    assert!(control.stops.load(Ordering::Acquire) > 0);
}

fn assert_lifecycle_timeout(error: &crate::error::DrasiError, accepted: bool) {
    match error.downcast_ref::<GraphError>() {
        Some(GraphError::LifecycleTimeout(timeout)) => {
            assert_eq!(timeout.accepted, accepted)
        }
        _ => panic!("expected a typed lifecycle timeout, got {error:#}"),
    }
}

#[tokio::test(start_paused = true)]
async fn startup_total_deadline_preserves_accepted_work_and_configuration() {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control.waiting_start.store(true, Ordering::Release);
    let core = builder()
        .with_source(source)
        .with_startup_timeout_secs(2)
        .with_shutdown_timeout_secs(3)
        .with_construction_timeout_secs(4)
        .with_resource_construction_timeout_secs(5)
        .with_cleanup_timeout_secs(6)
        .with_activation_timeout_secs(10)
        .build()
        .await
        .unwrap();
    let config = core.get_current_config().await.unwrap();
    assert_eq!(config.startup_timeout_secs, Some(2));
    assert_eq!(config.shutdown_timeout_secs, Some(3));
    assert_eq!(config.construction_timeout_secs, Some(4));
    assert_eq!(config.resource_construction_timeout_secs, Some(5));
    assert_eq!(config.cleanup_timeout_secs, Some(6));
    let began = tokio::time::Instant::now();
    let error = core.start().await.unwrap_err();
    assert_eq!(began.elapsed(), Duration::from_secs(2));
    assert!(error
        .downcast_ref::<crate::error::PartialStartError>()
        .is_some());
    assert_lifecycle_timeout(&error, true);
    assert!(core.is_running().await);
    control.release.notify_one();
    core.computation_component("source")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn non_startup_lifecycle_lock_wait_has_its_own_unaccepted_outcome() {
    let core = builder()
        .with_shutdown_timeout_secs(1)
        .build()
        .await
        .unwrap();
    let guard = core.instance_graph.lifecycle.lock().await;
    let began = tokio::time::Instant::now();
    let error = core.stop().await.unwrap_err();
    assert_eq!(began.elapsed(), Duration::from_secs(1));
    assert_lifecycle_timeout(&error, false);
    drop(guard);
    core.shutdown().await.unwrap();
}

async fn stop_interrupts_startup(drop_caller: bool) {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control.waiting_start.store(true, Ordering::Release);
    let core = builder()
        .with_source(source)
        .with_startup_timeout_secs(60)
        .with_activation_timeout_secs(60)
        .with_shutdown_timeout_secs(2)
        .build()
        .await
        .unwrap();
    let mut starting = Box::pin(core.start());
    tokio::select! {
        _ = control.entered.notified() => {}
        result = &mut starting => panic!("startup escaped its gate: {result:?}"),
    }
    assert!(core
        .get_graph()
        .await
        .unwrap()
        .get_component("source")
        .is_some());
    let starting = if drop_caller {
        drop(starting);
        None
    } else {
        Some(starting)
    };
    tokio::time::timeout(Duration::from_secs(3), core.stop())
        .await
        .expect("stop must not wait for the 60-second startup deadline")
        .expect("stopping the gated startup must finish cleanup");
    assert!(!core.is_running().await);
    assert!(control.stops.load(Ordering::Acquire) > 0);
    if let Some(starting) = starting {
        let error = starting.await.unwrap_err();
        assert!(error
            .downcast_ref::<crate::error::PartialStartError>()
            .is_some());
        assert!(format!("{error:#}").contains("superseded"));
    }
    control.release.notify_one();
    tokio::task::yield_now().await;
    assert!(
        !core.is_running().await,
        "a late startup waiter must not resurrect the instance"
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_interrupts_unpolled_or_dropped_startup_current_thread() {
    for dropped in [false, true] {
        stop_interrupts_startup(dropped).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_interrupts_unpolled_or_dropped_startup_multi_thread() {
    for dropped in [false, true] {
        stop_interrupts_startup(dropped).await;
    }
}

#[tokio::test]
async fn owned_startup_reports_a_panicking_hook_and_remains_stoppable() {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control
        .panic_subscription_fence
        .store(true, Ordering::Release);
    let core = builder().with_source(source).build().await.unwrap();
    let error = core.start().await.unwrap_err();
    assert!(error
        .downcast_ref::<crate::error::PartialStartError>()
        .is_some());
    assert!(error
        .downcast_ref::<crate::computation::v1::ComponentPanic>()
        .is_some());
    assert!(format!("{error:#}").contains("injected startup subscription panic"));
    core.stop().await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stop_total_deadline_retains_cleanup_and_blocks_restart_until_retry() {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    let core = builder()
        .with_source(source)
        .with_shutdown_timeout_secs(1)
        .with_cleanup_timeout_secs(30)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    control.pending_stop.store(true, Ordering::Release);
    let began = tokio::time::Instant::now();
    let error = core.stop().await.unwrap_err();
    assert_eq!(began.elapsed(), Duration::from_secs(1));
    assert_lifecycle_timeout(&error, true);
    assert!(core
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("cleanup"));
    control.pending_stop.store(false, Ordering::Release);
    control.stop_release.notify_one();
    core.stop().await.unwrap();
    core.start().await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_total_deadline_retains_the_driver_for_a_later_join() {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    let core = builder()
        .with_source(source)
        .with_shutdown_timeout_secs(1)
        .with_cleanup_timeout_secs(30)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    control.pending_stop.store(true, Ordering::Release);
    let began = tokio::time::Instant::now();
    let error = core.shutdown().await.unwrap_err();
    assert_eq!(began.elapsed(), Duration::from_secs(1));
    assert_lifecycle_timeout(&error, true);
    assert!(core.start().await.is_err());
    control.pending_stop.store(false, Ordering::Release);
    control.stop_release.notify_one();
    core.shutdown().await.unwrap();
    assert!(!core.is_running().await);
}
#[tokio::test(start_paused = true)]
async fn component_handle_stop_deadline_retains_its_active_operation() {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    let core = builder()
        .with_source(source)
        .with_shutdown_timeout_secs(1)
        .with_cleanup_timeout_secs(30)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    control.pending_stop.store(true, Ordering::Release);
    let handle = core.computation_component("source").unwrap();
    let began = tokio::time::Instant::now();
    match handle.stop().await.unwrap_err() {
        GraphError::LifecycleTimeout(timeout) => {
            assert!(timeout.accepted);
            assert!(timeout.owner.ends_with("/source"));
        }
        error => panic!("expected component timeout: {error:#}"),
    }
    assert_eq!(began.elapsed(), Duration::from_secs(1));
    control.pending_stop.store(false, Ordering::Release);
    control.stop_release.notify_one();
    handle.stop().await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn partial_start_reports_source_and_query_failures_and_remains_stoppable() {
    for failing in ["source", "query", "path-bounds"] {
        let (source, control) = ControlledSource::new("source");
        control.auto_start.store(true, Ordering::Release);
        if failing == "source" {
            control.fail_starts.store(1, Ordering::Release);
        }
        let mut query = config("query", Some("source"));
        query.auto_start = true;
        if failing == "query" {
            query.query = "THIS IS NOT A QUERY".into();
        } else if failing == "path-bounds" {
            query.query = "MATCH (a)-[:LINK*1..3]->(b) RETURN b".into();
        }
        let core = builder()
            .with_source(source)
            .with_query(query)
            .build()
            .await
            .unwrap();
        let view = core.component_graph();
        let error = core
            .start()
            .await
            .expect_err("requested startup failure must be returned");
        assert!(
            error
                .downcast_ref::<crate::error::PartialStartError>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            core.is_running().await,
            "partially started owners remain stoppable"
        );
        assert_instance_snapshot_status(&core, &view, ComponentStatus::Running).await;
        core.stop().await.unwrap();
        assert!(!core.is_running().await);
        assert_instance_snapshot_status(&core, &view, ComponentStatus::Stopped).await;
        core.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn adding_an_auto_start_query_during_instance_start_waits_for_the_lifecycle_boundary() {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control.waiting_start.store(true, Ordering::Release);
    let core = builder().with_source(source).build().await.unwrap();
    let starting = core.start();
    tokio::pin!(starting);
    tokio::select! {
        _ = control.entered.notified() => {}
        result = &mut starting => panic!("startup finished before its held source: {result:?}"),
    }
    let mut query = config("query", Some("source"));
    query.auto_start = true;
    let adding = core.add_query_with_handle(query);
    tokio::pin!(adding);
    assert!(futures::poll!(&mut adding).is_pending());
    assert!(core.list_queries().await.unwrap().is_empty());
    control.release.notify_one();
    starting.await.unwrap();
    let handle = adding.await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        core.get_query_status("query").await.unwrap(),
        ComponentStatus::Running
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn quiesced_public_status_is_reconfiguring_not_running() {
    let core = builder().build().await.unwrap();
    let (source, _) = ControlledSource::new("source");
    let handle = core.add_source_with_handle(source).await.unwrap();
    handle.wait_created().await.unwrap();
    let mut observed = handle.observed().unwrap();
    observed.lifecycle_requested = true;
    observed.realization = RealizationState::Created;
    for lifecycle in [ComponentLifecycle::Quiescing, ComponentLifecycle::Quiesced] {
        observed.lifecycle = lifecycle;
        assert_eq!(
            events::observed_status(&observed, ComponentStatus::Added),
            ComponentStatus::Reconfiguring,
        );
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn quiesced_queries_keep_generation_fenced_snapshot_and_history_reads() {
    let (source, _) = ControlledSource::new("source");
    let core = builder()
        .with_source(source)
        .with_query(config("query", Some("source")))
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    let query = core
        .query_manager()
        .get_query_instance("query")
        .await
        .unwrap();
    let mut subscription = query
        .subscribe("paused-reader".into())
        .await
        .unwrap()
        .receiver;
    insert(&core, "source").await;
    tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .unwrap()
        .unwrap();
    let before = query.fetch_snapshot().await.unwrap();
    let control = core.computation_control().unwrap();
    control
        .quiesce_components(
            control.desired_snapshot().revision,
            GraphSelection::Exact(vec![ComponentId::try_new("query").unwrap()]),
        )
        .await
        .unwrap();
    assert_eq!(
        core.get_query_status("query").await.unwrap(),
        ComponentStatus::Reconfiguring
    );
    let paused = query.fetch_snapshot().await.unwrap();
    assert_eq!(paused.to_vec(), before.to_vec());
    assert_eq!(paused.output_generation, before.output_generation);
    assert_eq!(query.fetch_outbox(0).await.unwrap().results.len(), 1);
    core.shutdown().await.unwrap();
}

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
                    .from_input("source")
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

async fn query_component_ownership() {
    let (source, source_calls) = ControlledSource::new("source");
    source_calls.auto_start.store(true, Ordering::Release);
    let (mut left_reaction, left_calls, mut left_output) =
        ControlledReaction::new("left-output", &["left"]);
    left_reaction.base = ReactionBase::new(
        ReactionBaseParams::new("left-output", vec!["left".into()]).with_auto_start(true),
    );
    let (mut right_reaction, right_calls, mut right_output) =
        ControlledReaction::new("right-output", &["right"]);
    right_reaction.base = ReactionBase::new(
        ReactionBaseParams::new("right-output", vec!["right".into()]).with_auto_start(true),
    );
    let query = |id| {
        crate::Query::cypher(id)
            .query("MATCH (n:Person) RETURN n.name AS name")
            .from_input("source")
            .enable_bootstrap(false)
            .build()
    };
    let core = builder()
        .with_id("query-ownership")
        .with_source(source)
        .with_query(query("left"))
        .with_query(query("right"))
        .with_reaction(left_reaction)
        .with_reaction(right_reaction)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    for id in ["source", "left", "right", "left-output", "right-output"] {
        core.computation_component(id)
            .unwrap()
            .wait_started()
            .await
            .unwrap();
    }
    let root = core.computation_control().unwrap();
    let root_id = root.desired_snapshot().id.clone();
    let left = core.computation_component("left").unwrap();
    let right = core.computation_component("right").unwrap();
    let right_graph = core.inspect_query_computation("right").await.unwrap();
    let before = right_graph.snapshot();
    let right_members = crate::computation::runtime::native_query::query_members(
        &root.registry_snapshot(),
        right.id(),
    );
    let right_resources = before
        .desired
        .select(GraphSelection::Exact(
            right_members.keys().cloned().collect(),
        ))
        .unwrap()
        .resources;
    let inventory = core.inspect_computation_inventory().await.unwrap();
    assert_eq!(
        inventory
            .scopes
            .values()
            .filter(|scope| scope.owner.is_none())
            .count(),
        1
    );
    assert_eq!(inventory.scopes.len(), 1);
    for id in ["left", "right"] {
        let node = ComponentId::try_new(id).unwrap();
        let scope = &inventory.scopes[&ComputationScope::root(root_id.clone())];
        assert!(scope.owner.is_none());
        assert!(scope
            .topology
            .nodes
            .contains_key(&GraphEntityId::Component(node.clone())));
        assert_eq!(scope.topology.graph_id, root_id);
        assert_eq!(
            root.desired_snapshot().specifications[&node].role,
            ComponentRole::Query
        );
    }

    insert_person(&core, "source", "one", "Alice").await;
    for output in [&mut left_output, &mut right_output] {
        let result = next(output).await;
        assert_eq!(result.sequence, 1);
        assert!(
            matches!(&result.results[..], [crate::channels::ResultDiff::Add { data, .. }]
            if data == &serde_json::json!({"name": "Alice"}))
        );
    }
    core.remove_reaction("left-output", false).await.unwrap();
    core.remove_query("left").await.unwrap();
    assert!(left.observed().is_err());
    assert_eq!(left_calls.stops.load(Ordering::Acquire), 1);
    let inventory = core.inspect_computation_inventory().await.unwrap();
    assert_eq!(inventory.scopes.len(), 1);
    assert!(!inventory.scopes[&ComputationScope::root(root_id.clone())]
        .topology
        .nodes
        .contains_key(&GraphEntityId::Component(
            ComponentId::try_new("left").unwrap()
        )));
    assert_eq!(
        right.generation(),
        core.computation_component("right").unwrap().generation()
    );
    assert_eq!(source_calls.starts.load(Ordering::Acquire), 1);
    assert_eq!(source_calls.stops.load(Ordering::Acquire), 0);
    assert_eq!(right_calls.stops.load(Ordering::Acquire), 0);

    insert_person(&core, "source", "two", "Bob").await;
    let result = next(&mut right_output).await;
    assert_eq!(result.query_id, "right");
    assert_eq!(result.sequence, 2);
    assert!(
        matches!(&result.results[..], [crate::channels::ResultDiff::Add { data, .. }]
        if data == &serde_json::json!({"name": "Bob"}))
    );
    let after = right_graph.snapshot();
    assert_eq!(after.desired.id, before.desired.id);
    assert!(after.desired.revision > before.desired.revision);
    assert_eq!(after.observed.run_epoch, before.observed.run_epoch);
    for (id, generation) in &right_members {
        assert_eq!(after.observed.components[id].generation, *generation);
    }
    for resource in &right_resources {
        assert_eq!(
            after.observed.resources[&resource.id].generation,
            before.observed.resources[&resource.id].generation
        );
    }

    core.add_query(query("left")).await.unwrap();
    let replacement = core.computation_component("left").unwrap();
    replacement.wait_started().await.unwrap();
    assert_ne!(replacement.generation(), left.generation());
    assert!(left.observed().is_err());
    let (mut reaction, _, mut replaced_output) =
        ControlledReaction::new("left-replacement-output", &["left"]);
    reaction.base = ReactionBase::new(
        ReactionBaseParams::new("left-replacement-output", vec!["left".into()])
            .with_auto_start(true),
    );
    core.add_reaction(reaction).await.unwrap();
    core.computation_component("left-replacement-output")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    insert_person(&core, "source", "three", "Carol").await;
    for (output, sequence) in [(&mut replaced_output, 1), (&mut right_output, 3)] {
        let result = next(output).await;
        assert_eq!(result.sequence, sequence);
        assert!(
            matches!(&result.results[..], [crate::channels::ResultDiff::Add { data, .. }]
            if data == &serde_json::json!({"name": "Carol"}))
        );
    }
    let inventory = core.inspect_computation_inventory().await.unwrap();
    assert_eq!(inventory.scopes.len(), 1);
    assert_eq!(
        inventory
            .scopes
            .values()
            .filter(|scope| scope.owner.is_none())
            .count(),
        1
    );
    let left_scope = &inventory.scopes[&ComputationScope::root(root_id.clone())];
    assert!(left_scope.owner.is_none());
    assert_eq!(
        root.observed().components[replacement.id()].generation,
        replacement.generation()
    );
    assert_eq!(
        core.computation_control().unwrap().desired_snapshot().id,
        root_id
    );
    assert_eq!(source_calls.initializations.load(Ordering::Acquire), 1);
    assert_eq!(source_calls.starts.load(Ordering::Acquire), 1);
    assert_eq!(source_calls.stops.load(Ordering::Acquire), 0);
    assert_eq!(right_calls.starts.load(Ordering::Acquire), 1);
    assert_eq!(right_calls.stops.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
    assert_eq!(source_calls.stops.load(Ordering::Acquire), 1);
    assert_eq!(right_calls.stops.load(Ordering::Acquire), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn query_component_ownership_and_sibling_progress_on_current_thread() {
    tokio::time::timeout(Duration::from_secs(15), query_component_ownership())
        .await
        .expect("query lifecycle must not stall unrelated query execution");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_component_ownership_and_sibling_progress_on_multiple_threads() {
    tokio::time::timeout(Duration::from_secs(15), query_component_ownership())
        .await
        .expect("query lifecycle must not stall unrelated query execution");
}

async fn many_queries_share_one_graph() {
    for count in [1, 8, 32] {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (source, control) = ControlledSource::new("shared-input");
            control.auto_start.store(true, Ordering::Release);
            let mut builder = builder().with_source(source);
            for index in 0..count {
                let mut query = config(&format!("query-{index}"), Some("shared-input"));
                query.auto_start = true;
                builder = builder.with_query(query);
            }
            let core = builder.build().await.unwrap();
            let started = std::time::Instant::now();
            core.start().await.unwrap();
            let mut receivers = Vec::new();
            for index in 0..count {
                let id = format!("query-{index}");
                core.computation_component(&id)
                    .unwrap()
                    .wait_started()
                    .await
                    .unwrap();
                let query = core.query_manager().get_query_instance(&id).await.unwrap();
                receivers.push((
                    id,
                    query
                        .subscribe("fanout-check".into())
                        .await
                        .unwrap()
                        .receiver,
                ));
            }
            let startup = started.elapsed();
            let processing = std::time::Instant::now();
            for sequence in 1..=32 {
                insert_person(
                    &core,
                    "shared-input",
                    &format!("person-{sequence}"),
                    &format!("name-{sequence}"),
                )
                .await;
            }
            for (id, mut receiver) in receivers {
                for sequence in 1..=32 {
                    let result = receiver.recv().await.unwrap();
                    assert_eq!(result.query_id, id);
                    assert_eq!(result.sequence, sequence);
                    assert!(matches!(&result.results[..],
                        [crate::channels::ResultDiff::Add { data, .. }]
                        if data == &serde_json::json!({ "name": format!("name-{sequence}") })));
                }
            }
            let processing = processing.elapsed();
            let inventory = core.inspect_computation_inventory().await.unwrap();
            assert_eq!(inventory.scopes.len(), 1);
            assert_eq!(core.list_queries().await.unwrap().len(), count);
            tracing::info!(
                queries = count,
                events_per_query = 32,
                ?startup,
                ?processing,
                "main-graph query fanout qualification"
            );
            core.shutdown().await.unwrap();
            assert_eq!(control.starts.load(Ordering::Acquire), 1);
            assert_eq!(control.stops.load(Ordering::Acquire), 1);
        })
        .await
        .expect("main-graph query fanout exceeded its responsiveness budget");
    }
}

#[tokio::test]
async fn many_queries_share_one_graph_current_thread() {
    many_queries_share_one_graph().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_queries_share_one_graph_multi_thread() {
    many_queries_share_one_graph().await;
}

#[tokio::test]
async fn direct_and_factory_additions_preserve_one_root_and_existing_query_components() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let core = builder()
            .with_query(config("query", None))
            .build()
            .await
            .unwrap();
        let query = core.computation_component("query").unwrap();
        query.wait_created().await.unwrap();
        let root = core.computation_control().unwrap();
        let root_id = root.desired_snapshot().id.clone();
        let query_graph = core.inspect_query_computation("query").await.unwrap();
        let before = query_graph.snapshot();
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(NativeServiceFactory {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/root-membership", "1")
                    .unwrap(),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::new(),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            starts: starts.clone(),
            stops: stops.clone(),
            index_providers: Vec::new(),
        });
        for name in ["before-start", "after-start"] {
            let descriptor =
                ComponentDescriptor::try_new(ComponentId::try_new(name).unwrap(), vec![]).unwrap();
            let addition = if name == "before-start" {
                ComponentAddition::new(ConstructedComponent::service(Box::new(NativeService {
                    descriptor,
                    starts: starts.clone(),
                    stops: stops.clone(),
                })))
            } else {
                ComponentAddition::from_specification(
                    ComponentSpecification {
                        descriptor,
                        role: ComponentRole::Service,
                        completion: None,
                        implementation: factory.descriptor().implementation.clone(),
                        configuration_version: 1,
                        configuration: BTreeMap::new(),
                        dependencies: BTreeMap::new(),
                    },
                    factory.clone(),
                )
            };
            let handle = core.add_computation_component(addition).await.unwrap();
            handle.wait_created().await.unwrap();
            if name == "before-start" {
                assert_eq!(starts.load(Ordering::Acquire), 0);
                core.start().await.unwrap();
            }
            handle.wait_started().await.unwrap();
            assert_eq!(
                root.desired_snapshot()
                    .nodes
                    .iter()
                    .filter(|node| node.descriptor.id() == handle.id())
                    .count(),
                1
            );
        }
        assert_eq!(starts.load(Ordering::Acquire), 2);
        let duplicate = core.add_query(config("before-start", None)).await;
        assert!(
            duplicate.is_err(),
            "component identity must not depend on its type"
        );
        assert_eq!(root.desired_snapshot().id, root_id);
        assert_eq!(
            query.generation(),
            core.computation_component("query").unwrap().generation()
        );
        let after = query_graph.snapshot();
        assert_eq!(after.desired.id, before.desired.id);
        assert!(after.desired.revision > before.desired.revision);
        for (id, node) in &before.observed.components {
            assert_eq!(after.observed.components[id].generation, node.generation);
        }
        let inventory = core.inspect_computation_inventory().await.unwrap();
        assert_eq!(inventory.scopes.len(), 1);
        assert_eq!(
            inventory
                .scopes
                .values()
                .filter(|scope| scope.owner.is_none())
                .count(),
            1
        );
        core.shutdown().await.unwrap();
        assert_eq!(stops.load(Ordering::Acquire), 2);
    })
    .await
    .expect("root additions and cleanup must complete");
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
async fn stop_sweep_continues_past_a_record_superseded_after_it_was_listed() {
    let (source, source_control) = ControlledSource::new("source");
    let (first, first_control, _) = ControlledReaction::new("a-reaction", &["query"]);
    let (second, second_control, _) = ControlledReaction::new("b-reaction", &["query"]);
    let core = builder()
        .with_source(source)
        .with_query(config("query", Some("source")))
        .with_reaction(first)
        .with_reaction(second)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("a-reaction").await.unwrap();
    core.start_reaction("b-reaction").await.unwrap();
    for id in ["source", "query", "a-reaction", "b-reaction"] {
        core.computation_component(id)
            .unwrap()
            .wait_started()
            .await
            .unwrap();
    }

    // A sweep lists its records before stopping them, and a concurrent
    // removal may commit in between. The superseded record is listed first,
    // so the sweep must step past it to reach the live one.
    let runtime = &core.computation_runtime;
    let listed = runtime.current_records().await.unwrap();
    core.remove_reaction("a-reaction", false).await.unwrap();
    let failures = runtime
        .stop_records(
            listed
                .values()
                .filter(|record| record.value.instance().kind() == "reaction"),
        )
        .await;

    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        first_control.stops.load(Ordering::Acquire),
        1,
        "the removal stopped its own component; the sweep left it alone"
    );
    assert_eq!(second_control.stops.load(Ordering::Acquire), 1);
    core.stop().await.unwrap();
    assert_eq!(
        core.get_query_status("query").await.unwrap(),
        ComponentStatus::Stopped
    );
    assert_eq!(source_control.stops.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn incomplete_instance_stop_is_retried_until_every_component_stops() {
    let (source, control) = ControlledSource::new("source");
    let core = builder().with_source(source).build().await.unwrap();
    let view = core.component_graph();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.computation_component("source")
        .unwrap()
        .wait_started()
        .await
        .unwrap();

    control.fail_stop.store(true, Ordering::Release);
    core.stop()
        .await
        .expect_err("the source's stop failure is reported");
    assert!(!core.is_running().await);
    assert_instance_snapshot_status(&core, &view, ComponentStatus::Stopped).await;
    assert!(matches!(
        core.start().await,
        Err(crate::DrasiError::InvalidState { .. })
    ));

    control.fail_stop.store(false, Ordering::Release);
    core.stop()
        .await
        .expect("an incomplete stop is retried rather than reported as already stopped");
    assert_eq!(control.stops.load(Ordering::Acquire), 2);
    assert_instance_snapshot_status(&core, &view, ComponentStatus::Stopped).await;
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Stopped
    );
    assert!(
        matches!(
            core.stop().await,
            Err(crate::DrasiError::InvalidState { .. })
        ),
        "a completed stop is not repeated"
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
async fn successful_source_stop_must_reach_stopped_before_cleanup_is_accepted() {
    let (source, control) = ControlledSource::new("noop-stop");
    let core = builder().with_source(source).build().await.unwrap();
    core.start_source("noop-stop").await.unwrap();
    core.computation_component("noop-stop")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    control.noop_stop.store(true, Ordering::Release);
    assert!(core.stop_source("noop-stop").await.is_err());
    assert_eq!(
        core.get_source_status("noop-stop").await.unwrap(),
        ComponentStatus::Error
    );
    assert_eq!(control.stops.load(Ordering::Acquire), 1);
    control.noop_stop.store(false, Ordering::Release);
    core.stop_source("noop-stop").await.unwrap();
    assert_eq!(control.stops.load(Ordering::Acquire), 2);
    assert_eq!(
        core.get_source_status("noop-stop").await.unwrap(),
        ComponentStatus::Stopped
    );
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
