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

//! End-to-end lifecycle event tests.
//!
//! These tests exercise the full component lifecycle through the `DrasiLib` public API
//! while subscribing to `subscribe_all_component_events()` to validate that all expected
//! `ComponentEvent`s are correctly generated, propagated, and received.

#[cfg(test)]
mod tests {
    use crate::test_helpers::creation::{BuilderFixtures, CoreFixtures};
    use std::time::Duration;

    use tokio::sync::broadcast;

    use crate::builder::Query;
    use crate::channels::{ComponentEvent, ComponentStatus, ComponentType};
    use crate::lib_core::DrasiLib;
    use crate::reactions::tests::manager_tests::{create_test_mock_reaction, TestMockReaction};
    use crate::sources::tests::TestMockSource;

    /// Default timeout for waiting on events.
    const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Build a minimal DrasiLib instance for testing.
    async fn create_test_core() -> DrasiLib {
        DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("lifecycle-event-test")
            .build()
            .await
            .expect("Failed to build test DrasiLib")
    }

    /// Collect all events for a specific component until a predicate is satisfied.
    ///
    /// Returns the collected events in order. Panics on timeout.
    async fn collect_events_until(
        event_rx: &mut broadcast::Receiver<ComponentEvent>,
        component_id: drasi_core_types::ComponentId,
        stop_predicate: impl Fn(&[ComponentEvent]) -> bool,
        timeout: Duration,
    ) -> Vec<ComponentEvent> {
        let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected_clone = collected.clone();
        match tokio::time::timeout(timeout, async {
            loop {
                match event_rx.recv().await {
                    Ok(event) if event.component_id == component_id => {
                        let mut c = collected_clone.lock().unwrap();
                        c.push(event);
                        if stop_predicate(&c) {
                            return c.clone();
                        }
                    }
                    Ok(_) => continue, // skip events for other components
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        panic!("Event receiver lagged by {n} events — increase channel capacity");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        panic!("Event channel closed while collecting events for '{component_id}'");
                    }
                }
            }
        })
        .await
        {
            Ok(events) => events,
            Err(_) => {
                let c = collected.lock().unwrap();
                panic!(
                    "Timed out ({timeout:?}) collecting events for '{component_id}'. \
                     Collected so far: {c:?}",
                )
            }
        }
    }

    /// Assert that a collected event sequence matches the expected statuses and messages.
    fn assert_event_sequence(
        events: &[ComponentEvent],
        component_id: drasi_core_types::ComponentId,
        component_type: ComponentType,
        expected: &[(ComponentStatus, &str)],
    ) {
        assert_eq!(
            events.len(),
            expected.len(),
            "Expected {} events for '{component_id}', got {}.\nActual events: {:#?}",
            expected.len(),
            events.len(),
            events
                .iter()
                .map(|e| format!("{:?} {:?}", e.status, e.message))
                .collect::<Vec<_>>()
        );

        for (i, (event, (expected_status, expected_msg))) in
            events.iter().zip(expected.iter()).enumerate()
        {
            assert_eq!(
                event.component_id, component_id,
                "Event {i}: wrong component_id"
            );
            assert_eq!(
                event.component_type, component_type,
                "Event {i}: wrong component_type for '{component_id}'"
            );
            assert_eq!(
                event.status, *expected_status,
                "Event {i}: wrong status for '{component_id}'. Expected {expected_status:?}, got {:?}. Message: {:?}",
                event.status, event.message
            );
            assert!(
                event
                    .message
                    .as_deref()
                    .is_some_and(|m| m.contains(expected_msg)),
                "Event {i}: message mismatch for '{component_id}'. \
                 Expected message containing '{expected_msg}', got {:?}",
                event.message
            );
        }
    }

    // ========================================================================
    // Source lifecycle tests
    // ========================================================================

    #[tokio::test]
    async fn test_source_full_lifecycle_events() {
        let core = create_test_core().await;
        let mut event_rx = core.subscribe_all_component_events();

        let source = TestMockSource::fixture("test-src", false);
        let source_id = core
            .add_source_fixture_with_handle(source)
            .await
            .unwrap()
            .id()
            .clone();

        // Collect the "added" event
        let events = collect_events_until(
            &mut event_rx,
            source_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            source_id.clone(),
            ComponentType::Source,
            &[(ComponentStatus::Added, "added")],
        );

        // Start
        core.start_source_by_name(
            &drasi_core_types::ComponentName::try_new("test-src").expect("fixture component name"),
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            source_id.clone(),
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;
        assert!(
            events.iter().any(|e| e.status == ComponentStatus::Starting),
            "Expected a Starting event during start"
        );
        assert!(
            events.last().unwrap().status == ComponentStatus::Running,
            "Expected last start event to be Running"
        );

        // Stop
        core.stop_source_by_name(
            &drasi_core_types::ComponentName::try_new("test-src").expect("fixture component name"),
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            source_id.clone(),
            |evts| {
                evts.iter().any(|e| {
                    e.status == ComponentStatus::Stopped
                        && e.message.as_deref() != Some("Source added")
                })
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(
            events.iter().any(|e| e.status == ComponentStatus::Stopping),
            "Expected a Stopping event during stop"
        );
        assert!(
            events.last().unwrap().status == ComponentStatus::Stopped,
            "Expected last stop event to be Stopped"
        );

        // Remove
        core.remove_source_by_name(
            &drasi_core_types::ComponentName::try_new("test-src").expect("fixture component name"),
            false,
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            source_id.clone(),
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            source_id,
            ComponentType::Source,
            &[(ComponentStatus::Removed, "removed")],
        );
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // Query lifecycle tests
    // ========================================================================

    #[tokio::test]
    async fn test_query_full_lifecycle_events() {
        // Queries depend on sources, so add a source first.
        let source = TestMockSource::fixture("q-src", false);
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("query-lifecycle-test")
            .with_source_fixture(source)
            .build()
            .await
            .unwrap();

        let source_id = core
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("q-src").expect("fixture component name"),
            )
            .unwrap()
            .id()
            .clone();
        let mut event_rx = core.subscribe_all_component_events();

        let query_config = Query::cypher(
            drasi_core_types::ComponentName::try_new("test-query").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("q-src").expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();

        let query_id = core
            .add_query_with_handle(query_config)
            .await
            .unwrap()
            .id()
            .clone();

        // Collect the "added" event
        let events = collect_events_until(
            &mut event_rx,
            query_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            query_id.clone(),
            ComponentType::Query,
            &[(ComponentStatus::Added, "added")],
        );

        // Start the source first (queries need running sources for bootstrap)
        core.start_source_by_name(
            &drasi_core_types::ComponentName::try_new("q-src").expect("fixture component name"),
        )
        .await
        .unwrap();
        // Drain source events
        let _ = collect_events_until(
            &mut event_rx,
            source_id,
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        // Start query
        core.start_query_by_name(
            &drasi_core_types::ComponentName::try_new("test-query")
                .expect("fixture component name"),
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            query_id.clone(),
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;
        assert!(
            events.iter().any(|e| e.status == ComponentStatus::Starting),
            "Expected a Starting event during query start"
        );
        assert!(
            events.last().unwrap().status == ComponentStatus::Running,
            "Expected last query start event to be Running"
        );

        // Stop query
        core.stop_query_by_name(
            &drasi_core_types::ComponentName::try_new("test-query")
                .expect("fixture component name"),
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            query_id.clone(),
            |evts| {
                evts.iter().any(|e| {
                    e.status == ComponentStatus::Stopped
                        && e.message.as_deref() != Some("Query added")
                })
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(
            events.last().unwrap().status == ComponentStatus::Stopped,
            "Expected last query stop event to be Stopped"
        );

        // Remove query
        core.remove_query_by_name(
            &drasi_core_types::ComponentName::try_new("test-query")
                .expect("fixture component name"),
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            query_id.clone(),
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            query_id,
            ComponentType::Query,
            &[(ComponentStatus::Removed, "removed")],
        );
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // Reaction lifecycle tests
    // ========================================================================

    #[tokio::test]
    async fn test_reaction_full_lifecycle_events() {
        // Reactions depend on queries, which depend on sources.
        let source = TestMockSource::fixture("r-src", false);
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("reaction-lifecycle-test")
            .with_source_fixture(source)
            .with_query(
                Query::cypher(
                    drasi_core_types::ComponentName::try_new("r-query")
                        .expect("fixture component name"),
                )
                .query("MATCH (n) RETURN n")
                .from_input_by_name(
                    drasi_core_types::ComponentName::try_new("r-src")
                        .expect("fixture component name"),
                )
                .auto_start(false)
                .build()
                .unwrap(),
            )
            .build()
            .await
            .unwrap();

        let mut event_rx = core.subscribe_all_component_events();

        let reaction = TestMockReaction::fixture(
            drasi_core_types::ComponentName::try_new("test-rxn").expect("fixture component name"),
            vec![drasi_core_types::ComponentName::try_new("r-query")
                .expect("fixture component name")],
            false,
        );
        let reaction_id = core
            .add_reaction_fixture_with_handle(reaction)
            .await
            .unwrap()
            .id()
            .clone();

        // Collect the "added" event
        let events = collect_events_until(
            &mut event_rx,
            reaction_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            reaction_id.clone(),
            ComponentType::Reaction,
            &[(ComponentStatus::Added, "added")],
        );

        core.start_source_by_name(
            &drasi_core_types::ComponentName::try_new("r-src").expect("fixture component name"),
        )
        .await
        .unwrap();
        core.start_query_by_name(
            &drasi_core_types::ComponentName::try_new("r-query").expect("fixture component name"),
        )
        .await
        .unwrap();

        // Start reaction
        core.start_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("test-rxn").expect("fixture component name"),
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            reaction_id.clone(),
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;
        assert!(
            events.iter().any(|e| e.status == ComponentStatus::Starting),
            "Expected a Starting event during reaction start"
        );
        assert!(
            events.last().unwrap().status == ComponentStatus::Running,
            "Expected last reaction start event to be Running"
        );

        // Stop reaction
        core.stop_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("test-rxn").expect("fixture component name"),
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            reaction_id.clone(),
            |evts| {
                evts.iter().any(|e| {
                    e.status == ComponentStatus::Stopped
                        && e.message.as_deref() != Some("Reaction added")
                })
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(
            events.iter().any(|e| e.status == ComponentStatus::Stopping),
            "Expected a Stopping event during reaction stop"
        );
        assert!(
            events.last().unwrap().status == ComponentStatus::Stopped,
            "Expected last reaction stop event to be Stopped"
        );

        // Remove reaction
        core.remove_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("test-rxn").expect("fixture component name"),
            false,
        )
        .await
        .unwrap();
        let events = collect_events_until(
            &mut event_rx,
            reaction_id.clone(),
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            reaction_id,
            ComponentType::Reaction,
            &[(ComponentStatus::Removed, "removed")],
        );
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // Multi-component lifecycle test
    // ========================================================================

    #[tokio::test]
    async fn test_multi_component_full_lifecycle_events() {
        let core = create_test_core().await;
        let mut event_rx = core.subscribe_all_component_events();

        // --- Add all components ---

        // Add source
        let source = TestMockSource::fixture("mc-src", false);
        let source_id = core
            .add_source_fixture_with_handle(source)
            .await
            .unwrap()
            .id()
            .clone();
        let src_added = collect_events_until(
            &mut event_rx,
            source_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_eq!(src_added[0].component_type, ComponentType::Source);
        assert_eq!(src_added[0].status, ComponentStatus::Added);

        // Add query (depends on source)
        let query_config = Query::cypher(
            drasi_core_types::ComponentName::try_new("mc-query").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("mc-src").expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        let query_id = core
            .add_query_with_handle(query_config)
            .await
            .unwrap()
            .id()
            .clone();
        let qry_added = collect_events_until(
            &mut event_rx,
            query_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_eq!(qry_added[0].component_type, ComponentType::Query);
        assert_eq!(qry_added[0].status, ComponentStatus::Added);

        // Add reaction (depends on query)
        let reaction = TestMockReaction::fixture(
            drasi_core_types::ComponentName::try_new("mc-rxn").expect("fixture component name"),
            vec![drasi_core_types::ComponentName::try_new("mc-query")
                .expect("fixture component name")],
            false,
        );
        let reaction_id = core
            .add_reaction_fixture_with_handle(reaction)
            .await
            .unwrap()
            .id()
            .clone();
        let rxn_added = collect_events_until(
            &mut event_rx,
            reaction_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_eq!(rxn_added[0].component_type, ComponentType::Reaction);
        assert_eq!(rxn_added[0].status, ComponentStatus::Added);

        // --- Start in dependency order: source → query → reaction ---

        core.start_source_by_name(
            &drasi_core_types::ComponentName::try_new("mc-src").expect("fixture component name"),
        )
        .await
        .unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            source_id.clone(),
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        core.start_query_by_name(
            &drasi_core_types::ComponentName::try_new("mc-query").expect("fixture component name"),
        )
        .await
        .unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            query_id.clone(),
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        core.start_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("mc-rxn").expect("fixture component name"),
        )
        .await
        .unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            reaction_id.clone(),
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        // --- Stop in reverse order: reaction → query → source ---

        core.stop_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("mc-rxn").expect("fixture component name"),
        )
        .await
        .unwrap();
        let rxn_stop = collect_events_until(
            &mut event_rx,
            reaction_id.clone(),
            |evts| {
                evts.iter().any(|e| {
                    e.status == ComponentStatus::Stopped
                        && e.message.as_deref() != Some("Reaction added")
                })
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(rxn_stop
            .iter()
            .any(|e| e.status == ComponentStatus::Stopping));

        core.stop_query_by_name(
            &drasi_core_types::ComponentName::try_new("mc-query").expect("fixture component name"),
        )
        .await
        .unwrap();
        let qry_stop = collect_events_until(
            &mut event_rx,
            query_id.clone(),
            |evts| {
                evts.iter().any(|e| {
                    e.status == ComponentStatus::Stopped
                        && e.message.as_deref() != Some("Query added")
                })
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(qry_stop.last().unwrap().status == ComponentStatus::Stopped);

        core.stop_source_by_name(
            &drasi_core_types::ComponentName::try_new("mc-src").expect("fixture component name"),
        )
        .await
        .unwrap();
        let src_stop = collect_events_until(
            &mut event_rx,
            source_id.clone(),
            |evts| {
                evts.iter().any(|e| {
                    e.status == ComponentStatus::Stopped
                        && e.message.as_deref() != Some("Source added")
                })
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(src_stop
            .iter()
            .any(|e| e.status == ComponentStatus::Stopping));

        // --- Remove in reverse dependency order: reaction → query → source ---

        core.remove_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("mc-rxn").expect("fixture component name"),
            false,
        )
        .await
        .unwrap();
        let rxn_removed = collect_events_until(
            &mut event_rx,
            reaction_id,
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(rxn_removed
            .last()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .contains("removed"));

        core.remove_query_by_name(
            &drasi_core_types::ComponentName::try_new("mc-query").expect("fixture component name"),
        )
        .await
        .unwrap();
        let qry_removed = collect_events_until(
            &mut event_rx,
            query_id,
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(qry_removed
            .last()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .contains("removed"));

        core.remove_source_by_name(
            &drasi_core_types::ComponentName::try_new("mc-src").expect("fixture component name"),
            false,
        )
        .await
        .unwrap();
        let src_removed = collect_events_until(
            &mut event_rx,
            source_id,
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert!(src_removed
            .last()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .contains("removed"));
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // Error-path lifecycle tests
    // ========================================================================

    #[tokio::test]
    async fn test_add_duplicate_source_fails() {
        let core = create_test_core().await;
        let s1 = TestMockSource::fixture("dup-src", false);
        core.add_source_fixture(s1).await.unwrap();

        let s2 = TestMockSource::fixture("dup-src", false);
        let result = core.add_source_fixture(s2).await;
        assert!(result.is_err(), "Adding duplicate source should fail");
    }

    #[tokio::test]
    async fn test_add_duplicate_query_fails() {
        let source = TestMockSource::fixture("dup-q-src", false);
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("dup-query-test")
            .with_source_fixture(source)
            .build()
            .await
            .unwrap();

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("dup-q").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("dup-q-src").expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        core.add_query(config.clone()).await.unwrap();

        let config2 = Query::cypher(
            drasi_core_types::ComponentName::try_new("dup-q").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("dup-q-src").expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        let result = core.add_query(config2).await;
        assert!(result.is_err(), "Adding duplicate query should fail");
    }

    #[tokio::test]
    async fn test_add_duplicate_reaction_fails() {
        let core = create_test_core().await;
        let r1 = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("dup-rxn").expect("fixture component name"),
            vec![],
        );
        core.add_reaction_fixture(r1).await.unwrap();

        let r2 = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("dup-rxn").expect("fixture component name"),
            vec![],
        );
        let result = core.add_reaction_fixture(r2).await;
        assert!(result.is_err(), "Adding duplicate reaction should fail");
    }

    #[tokio::test]
    async fn test_start_already_running_source_fails() {
        let core = create_test_core().await;
        let source = TestMockSource::fixture("run-src", false);
        let source_id = core
            .add_source_fixture_with_handle(source)
            .await
            .unwrap()
            .id()
            .clone();

        let mut event_rx = core.subscribe_all_component_events();
        core.start_source_by_name(
            &drasi_core_types::ComponentName::try_new("run-src").expect("fixture component name"),
        )
        .await
        .unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            source_id,
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        let result = core
            .start_source_by_name(
                &drasi_core_types::ComponentName::try_new("run-src")
                    .expect("fixture component name"),
            )
            .await;
        assert!(
            result.is_err(),
            "Starting an already-running source should fail"
        );
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stop_already_stopped_source_fails() {
        let core = create_test_core().await;
        let source = TestMockSource::fixture("stop-src", false);
        core.add_source_fixture(source).await.unwrap();

        // Source is in Stopped state; stopping again should fail
        let result = core
            .stop_source_by_name(
                &drasi_core_types::ComponentName::try_new("stop-src")
                    .expect("fixture component name"),
            )
            .await;
        assert!(
            result.is_err(),
            "Stopping an already-stopped source should fail"
        );
    }

    #[tokio::test]
    async fn test_remove_source_with_dependent_query_fails() {
        let source = TestMockSource::fixture("dep-src", false);
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("dep-test")
            .with_source_fixture(source)
            .build()
            .await
            .unwrap();

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("dep-query").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("dep-src").expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        core.add_query(config).await.unwrap();

        // Source has a dependent query — removal should fail
        let result = core
            .remove_source_by_name(
                &drasi_core_types::ComponentName::try_new("dep-src")
                    .expect("fixture component name"),
                false,
            )
            .await;
        assert!(
            result.is_err(),
            "Removing a source with dependent queries should fail"
        );
    }

    #[tokio::test]
    async fn test_remove_query_with_dependent_reaction_fails() {
        let source = TestMockSource::fixture("dep2-src", false);
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("dep2-test")
            .with_source_fixture(source)
            .with_query(
                Query::cypher(
                    drasi_core_types::ComponentName::try_new("dep2-query")
                        .expect("fixture component name"),
                )
                .query("MATCH (n) RETURN n")
                .from_input_by_name(
                    drasi_core_types::ComponentName::try_new("dep2-src")
                        .expect("fixture component name"),
                )
                .auto_start(false)
                .build()
                .unwrap(),
            )
            .build()
            .await
            .unwrap();

        let reaction = TestMockReaction::fixture(
            drasi_core_types::ComponentName::try_new("dep2-rxn").expect("fixture component name"),
            vec![drasi_core_types::ComponentName::try_new("dep2-query")
                .expect("fixture component name")],
            false,
        );
        core.add_reaction_fixture(reaction).await.unwrap();

        // Query has a dependent reaction — removal should fail
        let result = core
            .remove_query_by_name(
                &drasi_core_types::ComponentName::try_new("dep2-query")
                    .expect("fixture component name"),
            )
            .await;
        assert!(
            result.is_err(),
            "Removing a query with dependent reactions should fail"
        );
    }

    #[tokio::test]
    async fn test_start_nonexistent_source_fails() {
        let core = create_test_core().await;
        let result = core
            .start_source_by_name(
                &drasi_core_types::ComponentName::try_new("no-such-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(result.is_err(), "Starting a nonexistent source should fail");
    }

    #[tokio::test]
    async fn test_stop_nonexistent_source_fails() {
        let core = create_test_core().await;
        let result = core
            .stop_source_by_name(
                &drasi_core_types::ComponentName::try_new("no-such-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(result.is_err(), "Stopping a nonexistent source should fail");
    }

    #[tokio::test]
    async fn test_duplicate_event_subscription_receives_same_events() {
        let core = create_test_core().await;

        // Two independent subscriptions
        let mut rx1 = core.subscribe_all_component_events();
        let mut rx2 = core.subscribe_all_component_events();

        let source = TestMockSource::fixture("sub-src", false);
        let source_id = core
            .add_source_fixture_with_handle(source)
            .await
            .unwrap()
            .id()
            .clone();

        // Both receivers should get the "added" event
        let events1 = collect_events_until(
            &mut rx1,
            source_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        let events2 = collect_events_until(
            &mut rx2,
            source_id.clone(),
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;

        assert_eq!(events1[0].component_id, source_id);
        assert_eq!(events2[0].component_id, source_id);
        assert_eq!(events1[0].status, events2[0].status);
        core.shutdown().await.unwrap();
    }
}
