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
    use std::time::Duration;

    use tokio::sync::broadcast;

    use crate::builder::Query;
    use crate::channels::{ComponentEvent, ComponentStatus, ComponentType};
    use crate::lib_core::DrasiLib;
    use crate::reactions::tests::manager_tests::{create_test_mock_reaction, TestMockReaction};
    use crate::sources::tests::{create_test_mock_source, TestMockSource};

    /// Default timeout for waiting on events.
    const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Build a minimal DrasiLib instance for testing.
    async fn create_test_core() -> DrasiLib {
        DrasiLib::builder()
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
        component_id: &str,
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
        component_id: &str,
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

        let source = TestMockSource::with_auto_start("test-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        // Collect the "added" event
        let events = collect_events_until(
            &mut event_rx,
            "test-src",
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            "test-src",
            ComponentType::Source,
            &[(ComponentStatus::Added, "added")],
        );

        // Start
        core.start_source("test-src").await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-src",
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
        core.stop_source("test-src").await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-src",
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
        core.remove_source("test-src", false).await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-src",
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            "test-src",
            ComponentType::Source,
            &[(ComponentStatus::Removed, "removed")],
        );
    }

    // ========================================================================
    // Query lifecycle tests
    // ========================================================================

    #[tokio::test]
    async fn test_query_full_lifecycle_events() {
        // Queries depend on sources, so add a source first.
        let source = TestMockSource::with_auto_start("q-src".to_string(), false).unwrap();
        let core = DrasiLib::builder()
            .with_id("query-lifecycle-test")
            .with_source(source)
            .build()
            .await
            .unwrap();

        let mut event_rx = core.subscribe_all_component_events();

        let query_config = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("q-src")
            .auto_start(false)
            .build();

        core.add_query(query_config).await.unwrap();

        // Collect the "added" event
        let events = collect_events_until(
            &mut event_rx,
            "test-query",
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            "test-query",
            ComponentType::Query,
            &[(ComponentStatus::Added, "added")],
        );

        // Start the source first (queries need running sources for bootstrap)
        core.start_source("q-src").await.unwrap();
        // Drain source events
        let _ = collect_events_until(
            &mut event_rx,
            "q-src",
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        // Start query
        core.start_query("test-query").await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-query",
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
        core.stop_query("test-query").await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-query",
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
        core.remove_query("test-query").await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-query",
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            "test-query",
            ComponentType::Query,
            &[(ComponentStatus::Removed, "removed")],
        );
    }

    // ========================================================================
    // Reaction lifecycle tests
    // ========================================================================

    #[tokio::test]
    async fn test_reaction_full_lifecycle_events() {
        // Reactions depend on queries, which depend on sources.
        let source = TestMockSource::with_auto_start("r-src".to_string(), false).unwrap();
        let core = DrasiLib::builder()
            .with_id("reaction-lifecycle-test")
            .with_source(source)
            .with_query(
                Query::cypher("r-query")
                    .query("MATCH (n) RETURN n")
                    .from_source("r-src")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let mut event_rx = core.subscribe_all_component_events();

        let reaction = TestMockReaction::with_auto_start(
            "test-rxn".to_string(),
            vec!["r-query".to_string()],
            false,
        );
        core.add_reaction(reaction).await.unwrap();

        // Collect the "added" event
        let events = collect_events_until(
            &mut event_rx,
            "test-rxn",
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            "test-rxn",
            ComponentType::Reaction,
            &[(ComponentStatus::Added, "added")],
        );

        // Start reaction
        core.start_reaction("test-rxn").await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-rxn",
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
        core.stop_reaction("test-rxn").await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-rxn",
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
        core.remove_reaction("test-rxn", false).await.unwrap();
        let events = collect_events_until(
            &mut event_rx,
            "test-rxn",
            |evts| {
                evts.iter()
                    .any(|e| e.message.as_deref().is_some_and(|m| m.contains("removed")))
            },
            EVENT_TIMEOUT,
        )
        .await;
        assert_event_sequence(
            &events,
            "test-rxn",
            ComponentType::Reaction,
            &[(ComponentStatus::Removed, "removed")],
        );
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
        let source = TestMockSource::with_auto_start("mc-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();
        let src_added = collect_events_until(
            &mut event_rx,
            "mc-src",
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_eq!(src_added[0].component_type, ComponentType::Source);
        assert_eq!(src_added[0].status, ComponentStatus::Added);

        // Add query (depends on source)
        let query_config = Query::cypher("mc-query")
            .query("MATCH (n) RETURN n")
            .from_source("mc-src")
            .auto_start(false)
            .build();
        core.add_query(query_config).await.unwrap();
        let qry_added = collect_events_until(
            &mut event_rx,
            "mc-query",
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_eq!(qry_added[0].component_type, ComponentType::Query);
        assert_eq!(qry_added[0].status, ComponentStatus::Added);

        // Add reaction (depends on query)
        let reaction = TestMockReaction::with_auto_start(
            "mc-rxn".to_string(),
            vec!["mc-query".to_string()],
            false,
        );
        core.add_reaction(reaction).await.unwrap();
        let rxn_added = collect_events_until(
            &mut event_rx,
            "mc-rxn",
            |evts| evts.len() == 1,
            EVENT_TIMEOUT,
        )
        .await;
        assert_eq!(rxn_added[0].component_type, ComponentType::Reaction);
        assert_eq!(rxn_added[0].status, ComponentStatus::Added);

        // --- Start in dependency order: source → query → reaction ---

        core.start_source("mc-src").await.unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            "mc-src",
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        core.start_query("mc-query").await.unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            "mc-query",
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        core.start_reaction("mc-rxn").await.unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            "mc-rxn",
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        // --- Stop in reverse order: reaction → query → source ---

        core.stop_reaction("mc-rxn").await.unwrap();
        let rxn_stop = collect_events_until(
            &mut event_rx,
            "mc-rxn",
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

        core.stop_query("mc-query").await.unwrap();
        let qry_stop = collect_events_until(
            &mut event_rx,
            "mc-query",
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

        core.stop_source("mc-src").await.unwrap();
        let src_stop = collect_events_until(
            &mut event_rx,
            "mc-src",
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

        core.remove_reaction("mc-rxn", false).await.unwrap();
        let rxn_removed = collect_events_until(
            &mut event_rx,
            "mc-rxn",
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

        core.remove_query("mc-query").await.unwrap();
        let qry_removed = collect_events_until(
            &mut event_rx,
            "mc-query",
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

        core.remove_source("mc-src", false).await.unwrap();
        let src_removed = collect_events_until(
            &mut event_rx,
            "mc-src",
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
    }

    // ========================================================================
    // Error-path lifecycle tests
    // ========================================================================

    #[tokio::test]
    async fn test_add_duplicate_source_fails() {
        let core = create_test_core().await;
        let s1 = TestMockSource::with_auto_start("dup-src".to_string(), false).unwrap();
        core.add_source(s1).await.unwrap();

        let s2 = TestMockSource::with_auto_start("dup-src".to_string(), false).unwrap();
        let result = core.add_source(s2).await;
        assert!(result.is_err(), "Adding duplicate source should fail");
    }

    #[tokio::test]
    async fn test_add_duplicate_query_fails() {
        let source = TestMockSource::with_auto_start("dup-q-src".to_string(), false).unwrap();
        let core = DrasiLib::builder()
            .with_id("dup-query-test")
            .with_source(source)
            .build()
            .await
            .unwrap();

        let config = Query::cypher("dup-q")
            .query("MATCH (n) RETURN n")
            .from_source("dup-q-src")
            .auto_start(false)
            .build();
        core.add_query(config.clone()).await.unwrap();

        let config2 = Query::cypher("dup-q")
            .query("MATCH (n) RETURN n")
            .from_source("dup-q-src")
            .auto_start(false)
            .build();
        let result = core.add_query(config2).await;
        assert!(result.is_err(), "Adding duplicate query should fail");
    }

    #[tokio::test]
    async fn test_add_duplicate_reaction_fails() {
        let core = create_test_core().await;
        let r1 = create_test_mock_reaction("dup-rxn".to_string(), vec![]);
        core.add_reaction(r1).await.unwrap();

        let r2 = create_test_mock_reaction("dup-rxn".to_string(), vec![]);
        let result = core.add_reaction(r2).await;
        assert!(result.is_err(), "Adding duplicate reaction should fail");
    }

    #[tokio::test]
    async fn test_start_already_running_source_fails() {
        let core = create_test_core().await;
        let source = TestMockSource::with_auto_start("run-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        core.start_source("run-src").await.unwrap();
        let _ = collect_events_until(
            &mut event_rx,
            "run-src",
            |evts| evts.iter().any(|e| e.status == ComponentStatus::Running),
            EVENT_TIMEOUT,
        )
        .await;

        let result = core.start_source("run-src").await;
        assert!(
            result.is_err(),
            "Starting an already-running source should fail"
        );
    }

    #[tokio::test]
    async fn test_stop_already_stopped_source_fails() {
        let core = create_test_core().await;
        let source = TestMockSource::with_auto_start("stop-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        // Source is in Stopped state; stopping again should fail
        let result = core.stop_source("stop-src").await;
        assert!(
            result.is_err(),
            "Stopping an already-stopped source should fail"
        );
    }

    #[tokio::test]
    async fn test_remove_source_with_dependent_query_fails() {
        let source = TestMockSource::with_auto_start("dep-src".to_string(), false).unwrap();
        let core = DrasiLib::builder()
            .with_id("dep-test")
            .with_source(source)
            .build()
            .await
            .unwrap();

        let config = Query::cypher("dep-query")
            .query("MATCH (n) RETURN n")
            .from_source("dep-src")
            .auto_start(false)
            .build();
        core.add_query(config).await.unwrap();

        // Source has a dependent query — removal should fail
        let result = core.remove_source("dep-src", false).await;
        assert!(
            result.is_err(),
            "Removing a source with dependent queries should fail"
        );
    }

    #[tokio::test]
    async fn test_remove_query_with_dependent_reaction_fails() {
        let source = TestMockSource::with_auto_start("dep2-src".to_string(), false).unwrap();
        let core = DrasiLib::builder()
            .with_id("dep2-test")
            .with_source(source)
            .with_query(
                Query::cypher("dep2-query")
                    .query("MATCH (n) RETURN n")
                    .from_source("dep2-src")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let reaction = TestMockReaction::with_auto_start(
            "dep2-rxn".to_string(),
            vec!["dep2-query".to_string()],
            false,
        );
        core.add_reaction(reaction).await.unwrap();

        // Query has a dependent reaction — removal should fail
        let result = core.remove_query("dep2-query").await;
        assert!(
            result.is_err(),
            "Removing a query with dependent reactions should fail"
        );
    }

    #[tokio::test]
    async fn test_start_nonexistent_source_fails() {
        let core = create_test_core().await;
        let result = core.start_source("no-such-source").await;
        assert!(result.is_err(), "Starting a nonexistent source should fail");
    }

    #[tokio::test]
    async fn test_stop_nonexistent_source_fails() {
        let core = create_test_core().await;
        let result = core.stop_source("no-such-source").await;
        assert!(result.is_err(), "Stopping a nonexistent source should fail");
    }

    #[tokio::test]
    async fn test_duplicate_event_subscription_receives_same_events() {
        let core = create_test_core().await;

        // Two independent subscriptions
        let mut rx1 = core.subscribe_all_component_events();
        let mut rx2 = core.subscribe_all_component_events();

        let source = TestMockSource::with_auto_start("sub-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        // Both receivers should get the "added" event
        let events1 =
            collect_events_until(&mut rx1, "sub-src", |evts| evts.len() == 1, EVENT_TIMEOUT).await;
        let events2 =
            collect_events_until(&mut rx2, "sub-src", |evts| evts.len() == 1, EVENT_TIMEOUT).await;

        assert_eq!(events1[0].component_id, "sub-src");
        assert_eq!(events2[0].component_id, "sub-src");
        assert_eq!(events1[0].status, events2[0].status);
    }
}
