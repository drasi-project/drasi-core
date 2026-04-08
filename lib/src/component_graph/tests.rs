use super::*;
use crate::channels::{ComponentEvent, ComponentStatus, ComponentType};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

fn create_test_graph() -> ComponentGraph {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    graph
}

fn source_node(id: &str) -> ComponentNode {
    ComponentNode {
        id: id.to_string(),
        kind: ComponentKind::Source,
        status: ComponentStatus::Stopped,
        metadata: HashMap::new(),
    }
}

fn query_node(id: &str) -> ComponentNode {
    ComponentNode {
        id: id.to_string(),
        kind: ComponentKind::Query,
        status: ComponentStatus::Stopped,
        metadata: HashMap::new(),
    }
}

fn reaction_node(id: &str) -> ComponentNode {
    ComponentNode {
        id: id.to_string(),
        kind: ComponentKind::Reaction,
        status: ComponentStatus::Stopped,
        metadata: HashMap::new(),
    }
}

#[test]
fn test_new_graph_has_instance_root() {
    let graph = create_test_graph();
    assert_eq!(graph.instance_id(), "test-instance");
    assert_eq!(graph.node_count(), 1);
    assert!(graph.contains("test-instance"));
}

#[test]
fn test_add_component() {
    let mut graph = create_test_graph();
    let result = graph.add_component(source_node("source-1"));
    assert!(result.is_ok());
    assert_eq!(graph.node_count(), 2);
    assert!(graph.contains("source-1"));

    // Should have 2 edges: Instance--Owns-->Source, Source--OwnedBy-->Instance
    assert_eq!(graph.edge_count(), 2);
}

#[test]
fn test_add_duplicate_component_fails() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    let result = graph.add_component(source_node("source-1"));
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already exists"));
}

#[test]
fn test_remove_component() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    assert_eq!(graph.node_count(), 2);

    let removed = graph.remove_component("source-1").unwrap();
    assert_eq!(removed.id, "source-1");
    assert_eq!(graph.node_count(), 1);
    assert!(!graph.contains("source-1"));
    // Edges should be removed too
    assert_eq!(graph.edge_count(), 0);
}

#[test]
fn test_cannot_remove_instance_root() {
    let mut graph = create_test_graph();
    let result = graph.remove_component("test-instance");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("instance root"));
}

#[test]
fn test_remove_nonexistent_fails() {
    let mut graph = create_test_graph();
    let result = graph.remove_component("nonexistent");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[test]
fn test_bidirectional_relationship() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();

    // Add Source --Feeds--> Query (and Query --SubscribesTo--> Source)
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();

    // 4 Owns/OwnedBy edges + 2 Feeds/SubscribesTo edges = 6
    assert_eq!(graph.edge_count(), 6);

    // Source feeds query
    let dependents = graph.get_dependents("source-1");
    assert_eq!(dependents.len(), 1);
    assert_eq!(dependents[0].id, "query-1");

    // Query subscribes to source
    let dependencies = graph.get_dependencies("query-1");
    assert_eq!(dependencies.len(), 1);
    assert_eq!(dependencies[0].id, "source-1");
}

#[test]
fn test_can_remove_with_dependents() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();

    // Cannot remove source with dependent query
    let result = graph.can_remove("source-1");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), vec!["query-1"]);

    // Can remove query (no dependents)
    assert!(graph.can_remove("query-1").is_ok());
}

#[test]
fn test_full_pipeline_source_query_reaction() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph.add_component(reaction_node("reaction-1")).unwrap();

    // Source --Feeds--> Query --Feeds--> Reaction
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    graph
        .add_relationship("query-1", "reaction-1", RelationshipKind::Feeds)
        .unwrap();

    // Source has query as dependent
    let source_deps = graph.get_dependents("source-1");
    assert_eq!(source_deps.len(), 1);
    assert_eq!(source_deps[0].id, "query-1");

    // Query has reaction as dependent
    let query_deps = graph.get_dependents("query-1");
    assert_eq!(query_deps.len(), 1);
    assert_eq!(query_deps[0].id, "reaction-1");

    // Reaction has no dependents
    assert!(graph.get_dependents("reaction-1").is_empty());

    // Reaction depends on query
    let reaction_deps = graph.get_dependencies("reaction-1");
    assert_eq!(reaction_deps.len(), 1);
    assert_eq!(reaction_deps[0].id, "query-1");
}

#[test]
fn test_update_status() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Stopped
    );

    // Follow valid transitions: Stopped → Starting → Running
    graph
        .update_status("source-1", ComponentStatus::Starting)
        .unwrap();

    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Starting
    );

    graph
        .update_status("source-1", ComponentStatus::Running)
        .unwrap();

    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Running
    );
}

#[test]
fn test_list_by_kind() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(source_node("source-2")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();

    let sources = graph.list_by_kind(&ComponentKind::Source);
    assert_eq!(sources.len(), 2);

    let queries = graph.list_by_kind(&ComponentKind::Query);
    assert_eq!(queries.len(), 1);

    let reactions = graph.list_by_kind(&ComponentKind::Reaction);
    assert!(reactions.is_empty());
}

#[test]
fn test_snapshot() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();

    let snapshot = graph.snapshot();
    assert_eq!(snapshot.instance_id, "test-instance");
    assert_eq!(snapshot.nodes.len(), 3); // instance + source + query
    assert_eq!(snapshot.edges.len(), 6); // 4 Owns/OwnedBy + 2 Feeds/SubscribesTo
}

#[test]
fn test_snapshot_serializes_to_json() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();

    let snapshot = graph.snapshot();
    let json = serde_json::to_string_pretty(&snapshot).unwrap();
    assert!(json.contains("test-instance"));
    assert!(json.contains("source-1"));
    assert!(json.contains("query-1"));
    assert!(json.contains("Feeds"));
    assert!(json.contains("SubscribesTo"));
}

#[test]
fn test_topological_order() {
    let mut graph = create_test_graph();
    graph.add_component(reaction_node("reaction-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph.add_component(source_node("source-1")).unwrap();

    // Source --Feeds--> Query --Feeds--> Reaction
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    graph
        .add_relationship("query-1", "reaction-1", RelationshipKind::Feeds)
        .unwrap();

    let order = graph.topological_order().unwrap();
    let ids: Vec<&str> = order.iter().map(|n| n.id.as_str()).collect();

    // Source must come before Query, Query must come before Reaction
    let source_pos = ids.iter().position(|&id| id == "source-1").unwrap();
    let query_pos = ids.iter().position(|&id| id == "query-1").unwrap();
    let reaction_pos = ids.iter().position(|&id| id == "reaction-1").unwrap();
    assert!(source_pos < query_pos);
    assert!(query_pos < reaction_pos);
}

#[test]
fn test_get_neighbors_by_relationship() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();

    // Source's Feeds neighbors = [query-1]
    let feeds = graph.get_neighbors("source-1", &RelationshipKind::Feeds);
    assert_eq!(feeds.len(), 1);
    assert_eq!(feeds[0].id, "query-1");

    // Source's Owns neighbors = [] (only Instance owns things)
    let owns = graph.get_neighbors("source-1", &RelationshipKind::Owns);
    assert!(owns.is_empty());

    // Instance's Owns neighbors = [source-1, query-1]
    let instance_owns = graph.get_neighbors("test-instance", &RelationshipKind::Owns);
    assert_eq!(instance_owns.len(), 2);
}

#[test]
fn test_multiple_sources_feed_same_query() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(source_node("source-2")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();

    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    graph
        .add_relationship("source-2", "query-1", RelationshipKind::Feeds)
        .unwrap();

    // Query depends on both sources
    let deps = graph.get_dependencies("query-1");
    assert_eq!(deps.len(), 2);
    let dep_ids: Vec<&str> = deps.iter().map(|n| n.id.as_str()).collect();
    assert!(dep_ids.contains(&"source-1"));
    assert!(dep_ids.contains(&"source-2"));
}

#[test]
fn test_remove_component_cleans_up_edges() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();

    // 6 edges total
    assert_eq!(graph.edge_count(), 6);

    // Remove query — its edges should be cleaned up
    graph.remove_component("query-1").unwrap();
    // Only 2 edges left: Instance--Owns/OwnedBy-->Source
    assert_eq!(graph.edge_count(), 2);

    // Source no longer has any dependents
    assert!(graph.get_dependents("source-1").is_empty());
}

// ====================================================================
// Duplicate edge prevention tests
// ====================================================================

#[test]
fn test_add_duplicate_relationship_is_idempotent() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();

    // First add: creates 2 new edges (Feeds + SubscribesTo)
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    assert_eq!(graph.edge_count(), 6); // 4 Owns/OwnedBy + 2 Feeds/SubscribesTo

    // Second add: no-op
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    assert_eq!(graph.edge_count(), 6); // unchanged
}

// ====================================================================
// Remove relationship tests
// ====================================================================

#[test]
fn test_remove_relationship() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    assert_eq!(graph.edge_count(), 6);

    // Remove the Feeds/SubscribesTo relationship
    graph
        .remove_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    // Only 4 Owns/OwnedBy edges remain
    assert_eq!(graph.edge_count(), 4);

    // No more dependents
    assert!(graph.get_dependents("source-1").is_empty());
    assert!(graph.get_dependencies("query-1").is_empty());
}

#[test]
fn test_remove_nonexistent_relationship_is_noop() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    // No relationship added — removal should succeed as no-op
    graph
        .remove_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    assert_eq!(graph.edge_count(), 4);
}

// ====================================================================
// Transaction tests
// ====================================================================

#[test]
fn test_transaction_commit_emits_events() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    let mut event_rx = graph.subscribe();

    {
        let mut txn = graph.begin();
        txn.add_component(source_node("source-1")).unwrap();
        txn.add_component(query_node("query-1")).unwrap();
        txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        // Events not yet emitted
        assert!(event_rx.try_recv().is_err());
        txn.commit();
    }

    // After commit, events are emitted
    let e1 = event_rx.try_recv().unwrap();
    assert_eq!(e1.component_id, "source-1");
    let e2 = event_rx.try_recv().unwrap();
    assert_eq!(e2.component_id, "query-1");

    // Graph has the components
    assert!(graph.contains("source-1"));
    assert!(graph.contains("query-1"));
    assert_eq!(graph.get_dependents("source-1").len(), 1);
}

#[test]
fn test_transaction_rollback_on_drop() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    {
        let mut txn = graph.begin();
        txn.add_component(source_node("source-1")).unwrap();
        txn.add_component(query_node("query-1")).unwrap();
        txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        // Drop without commit — rollback
    }

    // Components should not exist
    assert!(!graph.contains("source-1"));
    assert!(!graph.contains("query-1"));
    assert_eq!(graph.node_count(), 1); // only instance root
    assert_eq!(graph.edge_count(), 0);
}

#[test]
fn test_transaction_partial_failure_rollback() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    // Pre-add a source outside the transaction
    graph.add_component(source_node("source-1")).unwrap();
    assert_eq!(graph.node_count(), 2);

    {
        let mut txn = graph.begin();
        txn.add_component(query_node("query-1")).unwrap();
        // Try to add duplicate — this fails
        let result = txn.add_component(source_node("source-1"));
        assert!(result.is_err());
        // Drop without commit — query-1 should be rolled back
    }

    // source-1 still exists (added before transaction), query-1 does not
    assert!(graph.contains("source-1"));
    assert!(!graph.contains("query-1"));
    assert_eq!(graph.node_count(), 2);
}

#[test]
fn test_valid_state_transitions() {
    assert!(is_valid_transition(
        &ComponentStatus::Stopped,
        &ComponentStatus::Starting
    ));
    assert!(is_valid_transition(
        &ComponentStatus::Starting,
        &ComponentStatus::Running
    ));
    assert!(is_valid_transition(
        &ComponentStatus::Running,
        &ComponentStatus::Stopping
    ));
    assert!(is_valid_transition(
        &ComponentStatus::Stopping,
        &ComponentStatus::Stopped
    ));
    assert!(is_valid_transition(
        &ComponentStatus::Starting,
        &ComponentStatus::Error
    ));
    assert!(is_valid_transition(
        &ComponentStatus::Running,
        &ComponentStatus::Error
    ));
    assert!(is_valid_transition(
        &ComponentStatus::Error,
        &ComponentStatus::Starting
    ));
    assert!(is_valid_transition(
        &ComponentStatus::Error,
        &ComponentStatus::Stopped
    ));
}

#[test]
fn test_invalid_state_transitions() {
    assert!(!is_valid_transition(
        &ComponentStatus::Stopped,
        &ComponentStatus::Running
    ));
    assert!(!is_valid_transition(
        &ComponentStatus::Stopped,
        &ComponentStatus::Stopping
    ));
    assert!(!is_valid_transition(
        &ComponentStatus::Running,
        &ComponentStatus::Starting
    ));
    assert!(!is_valid_transition(
        &ComponentStatus::Starting,
        &ComponentStatus::Stopping
    ));
}

#[test]
fn test_update_status_rejects_invalid_transition() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    // source-1 starts as Stopped
    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Stopped
    );

    // Invalid: Stopped → Running (must go through Starting)
    let result = graph.update_status("source-1", ComponentStatus::Running);
    assert!(result.is_ok());
    // The update should return None (skipped) and status should remain Stopped
    assert!(result.unwrap().is_none());
    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Stopped
    );

    // Valid: Stopped → Starting
    let result = graph.update_status("source-1", ComponentStatus::Starting);
    assert!(result.is_ok());
    assert!(result.unwrap().is_some());
    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Starting
    );
}

// ====================================================================
// Registration method tests
// ====================================================================

#[test]
fn test_register_source() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();

    assert!(graph.contains("source-1"));
    assert_eq!(
        graph.get_component("source-1").unwrap().kind,
        ComponentKind::Source
    );
    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Stopped
    );
    // 2 ownership edges
    assert_eq!(graph.edge_count(), 2);
}

#[test]
fn test_register_source_duplicate_fails() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();
    let result = graph.register_source("source-1", HashMap::new());
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already exists"));
}

#[test]
fn test_register_query_with_sources() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();
    graph.register_source("source-2", HashMap::new()).unwrap();

    graph
        .register_query(
            "query-1",
            HashMap::new(),
            &["source-1".to_string(), "source-2".to_string()],
        )
        .unwrap();

    assert!(graph.contains("query-1"));
    let deps = graph.get_dependencies("query-1");
    assert_eq!(deps.len(), 2);
    assert_eq!(graph.get_dependents("source-1").len(), 1);
    assert_eq!(graph.get_dependents("source-2").len(), 1);
}

#[test]
fn test_register_query_missing_source_fails() {
    let mut graph = create_test_graph();
    let result = graph.register_query("query-1", HashMap::new(), &["source-1".to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("does not exist"));
    assert!(!graph.contains("query-1"));
}

#[test]
fn test_register_reaction_with_queries() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();
    graph
        .register_query("query-1", HashMap::new(), &["source-1".to_string()])
        .unwrap();

    graph
        .register_reaction("reaction-1", HashMap::new(), &["query-1".to_string()])
        .unwrap();

    assert!(graph.contains("reaction-1"));
    assert_eq!(graph.get_dependents("query-1").len(), 1);
    assert_eq!(graph.get_dependencies("reaction-1").len(), 1);
}

#[test]
fn test_register_reaction_missing_query_fails() {
    let mut graph = create_test_graph();
    let result = graph.register_reaction(
        "reaction-1",
        HashMap::new(),
        &["nonexistent-query".to_string()],
    );
    assert!(result.is_err());
    assert!(!graph.contains("reaction-1"));
}

#[test]
fn test_deregister_succeeds_no_dependents() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();
    let removed = graph.deregister("source-1").unwrap();
    assert_eq!(removed.id, "source-1");
    assert!(!graph.contains("source-1"));
}

#[test]
fn test_deregister_fails_with_dependents() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();
    graph
        .register_query("query-1", HashMap::new(), &["source-1".to_string()])
        .unwrap();

    let result = graph.deregister("source-1");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("depended on by"));
    assert!(graph.contains("source-1"));
}

#[test]
fn test_full_pipeline_registration() {
    let mut graph = create_test_graph();
    graph.register_source("src", HashMap::new()).unwrap();
    graph
        .register_query("qry", HashMap::new(), &["src".to_string()])
        .unwrap();
    graph
        .register_reaction("rxn", HashMap::new(), &["qry".to_string()])
        .unwrap();

    assert_eq!(graph.get_dependents("src").len(), 1);
    assert_eq!(graph.get_dependents("qry").len(), 1);
    assert!(graph.get_dependents("rxn").is_empty());

    assert!(graph.deregister("src").is_err());
    graph.deregister("rxn").unwrap();
    graph.deregister("qry").unwrap();
    graph.deregister("src").unwrap();

    assert_eq!(graph.node_count(), 1);
}

// ========================================================================
// validate_and_transition tests
// ========================================================================

#[test]
fn test_validate_and_transition_stopped_to_starting() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();

    let result = graph.validate_and_transition(
        "s1",
        ComponentStatus::Starting,
        Some("Starting source".into()),
    );
    assert!(result.is_ok());
    assert!(result.unwrap().is_some()); // event emitted
    assert_eq!(
        graph.get_component("s1").unwrap().status,
        ComponentStatus::Starting
    );
}

#[test]
fn test_validate_and_transition_running_to_stopping() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();
    // Move to Running first
    graph
        .validate_and_transition("s1", ComponentStatus::Starting, None)
        .unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Running, None)
        .unwrap();

    let result = graph.validate_and_transition(
        "s1",
        ComponentStatus::Stopping,
        Some("Stopping source".into()),
    );
    assert!(result.is_ok());
    assert_eq!(
        graph.get_component("s1").unwrap().status,
        ComponentStatus::Stopping
    );
}

#[test]
fn test_validate_and_transition_idempotent_same_state() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();

    let result = graph.validate_and_transition("s1", ComponentStatus::Stopped, None);
    assert!(result.is_ok());
    assert!(result.unwrap().is_none()); // no event for same-state
}

#[test]
fn test_validate_and_transition_invalid_start_while_running() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Starting, None)
        .unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Running, None)
        .unwrap();

    let result = graph.validate_and_transition("s1", ComponentStatus::Starting, None);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("already running"),
        "Expected 'already running' in: {err_msg}"
    );
    // Status unchanged
    assert_eq!(
        graph.get_component("s1").unwrap().status,
        ComponentStatus::Running
    );
}

#[test]
fn test_validate_and_transition_invalid_stop_while_stopped() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();

    let result = graph.validate_and_transition("s1", ComponentStatus::Stopping, None);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("already stopped"),
        "Expected 'already stopped' in: {err_msg}"
    );
}

#[test]
fn test_validate_and_transition_error_recovery() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Starting, None)
        .unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Error, None)
        .unwrap();

    // Error → Starting (retry) should work
    let result = graph.validate_and_transition("s1", ComponentStatus::Starting, None);
    assert!(result.is_ok());
    assert_eq!(
        graph.get_component("s1").unwrap().status,
        ComponentStatus::Starting
    );
}

#[test]
fn test_validate_and_transition_reconfiguring() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();

    // Stopped → Reconfiguring should work
    let result = graph.validate_and_transition("s1", ComponentStatus::Reconfiguring, None);
    assert!(result.is_ok());
    assert_eq!(
        graph.get_component("s1").unwrap().status,
        ComponentStatus::Reconfiguring
    );

    // Reconfiguring → Stopped should work
    let result = graph.validate_and_transition("s1", ComponentStatus::Stopped, None);
    assert!(result.is_ok());
}

#[test]
fn test_validate_and_transition_invalid_reconfig_while_stopping() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Starting, None)
        .unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Running, None)
        .unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Stopping, None)
        .unwrap();

    let result = graph.validate_and_transition("s1", ComponentStatus::Reconfiguring, None);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("stopping"),
        "Expected 'stopping' in: {err_msg}"
    );
}

#[test]
fn test_validate_and_transition_nonexistent_component() {
    let mut graph = create_test_graph();

    let result = graph.validate_and_transition("nonexistent", ComponentStatus::Starting, None);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[test]
fn test_validate_and_transition_cannot_stop_error_state() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("s1")).unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Starting, None)
        .unwrap();
    graph
        .validate_and_transition("s1", ComponentStatus::Error, None)
        .unwrap();

    let result = graph.validate_and_transition("s1", ComponentStatus::Stopping, None);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("error state"),
        "Expected 'error state' in: {err_msg}"
    );
}

// ========================================================================
// register_bootstrap_provider tests
// ========================================================================

#[test]
fn test_register_bootstrap_provider_standalone() {
    let mut graph = create_test_graph();
    graph
        .register_bootstrap_provider("bp-1", HashMap::new(), &[])
        .unwrap();

    assert!(graph.contains("bp-1"));
    assert_eq!(
        graph.get_component("bp-1").unwrap().kind,
        ComponentKind::BootstrapProvider
    );
    assert_eq!(
        graph.get_component("bp-1").unwrap().status,
        ComponentStatus::Stopped
    );
    // 2 ownership edges (Owns + OwnedBy)
    assert_eq!(graph.edge_count(), 2);
}

#[test]
fn test_register_bootstrap_provider_with_source() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();
    graph
        .register_bootstrap_provider("bp-1", HashMap::new(), &["source-1".to_string()])
        .unwrap();

    assert!(graph.contains("bp-1"));
    // 2 ownership edges for source + 2 ownership edges for bp + 2 Bootstraps/BootstrappedBy edges
    assert_eq!(graph.edge_count(), 6);
}

#[test]
fn test_register_bootstrap_provider_duplicate_fails() {
    let mut graph = create_test_graph();
    graph
        .register_bootstrap_provider("bp-1", HashMap::new(), &[])
        .unwrap();
    let result = graph.register_bootstrap_provider("bp-1", HashMap::new(), &[]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already exists"));
}

#[test]
fn test_register_bootstrap_provider_missing_source_fails() {
    let mut graph = create_test_graph();
    let result = graph.register_bootstrap_provider(
        "bp-1",
        HashMap::new(),
        &["nonexistent-source".to_string()],
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("does not exist"));
    assert!(!graph.contains("bp-1"));
}

// ========================================================================
// register_identity_provider tests
// ========================================================================

#[test]
fn test_register_identity_provider_standalone() {
    let mut graph = create_test_graph();
    graph
        .register_identity_provider("ip-1", HashMap::new(), &[])
        .unwrap();

    assert!(graph.contains("ip-1"));
    assert_eq!(
        graph.get_component("ip-1").unwrap().kind,
        ComponentKind::IdentityProvider
    );
    assert_eq!(
        graph.get_component("ip-1").unwrap().status,
        ComponentStatus::Stopped
    );
    // 2 ownership edges (Owns + OwnedBy)
    assert_eq!(graph.edge_count(), 2);
}

#[test]
fn test_register_identity_provider_with_components() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();
    graph
        .register_identity_provider("ip-1", HashMap::new(), &["source-1".to_string()])
        .unwrap();

    assert!(graph.contains("ip-1"));
    // 2 ownership edges for source + 2 ownership edges for ip + 2 Authenticates/AuthenticatedBy edges
    assert_eq!(graph.edge_count(), 6);
}

#[test]
fn test_register_identity_provider_duplicate_fails() {
    let mut graph = create_test_graph();
    graph
        .register_identity_provider("ip-1", HashMap::new(), &[])
        .unwrap();
    let result = graph.register_identity_provider("ip-1", HashMap::new(), &[]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already exists"));
}

#[test]
fn test_register_identity_provider_missing_component_fails() {
    let mut graph = create_test_graph();
    let result = graph.register_identity_provider(
        "ip-1",
        HashMap::new(),
        &["nonexistent-source".to_string()],
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("does not exist"));
    assert!(!graph.contains("ip-1"));
}

#[test]
fn test_full_pipeline_with_providers() {
    let mut graph = create_test_graph();

    // Register providers first
    graph
        .register_bootstrap_provider("bp-1", HashMap::new(), &[])
        .unwrap();
    graph
        .register_identity_provider("ip-1", HashMap::new(), &[])
        .unwrap();

    // Register source with bootstrap and identity links
    graph.register_source("src", HashMap::new()).unwrap();
    graph
        .add_relationship("bp-1", "src", RelationshipKind::Bootstraps)
        .unwrap();
    graph
        .add_relationship("ip-1", "src", RelationshipKind::Authenticates)
        .unwrap();

    // Register query and reaction
    graph
        .register_query("qry", HashMap::new(), &["src".to_string()])
        .unwrap();
    graph
        .register_reaction("rxn", HashMap::new(), &["qry".to_string()])
        .unwrap();

    // Verify topology
    assert_eq!(graph.node_count(), 6); // instance + bp + ip + src + qry + rxn

    // List by kind
    assert_eq!(
        graph.list_by_kind(&ComponentKind::BootstrapProvider).len(),
        1
    );
    assert_eq!(
        graph.list_by_kind(&ComponentKind::IdentityProvider).len(),
        1
    );

    // Teardown in reverse order
    graph.deregister("rxn").unwrap();
    graph.deregister("qry").unwrap();
    graph.deregister("src").unwrap();
    graph.deregister("ip-1").unwrap();
    graph.deregister("bp-1").unwrap();

    assert_eq!(graph.node_count(), 1); // only instance remains
}

// ========================================================================
// Runtime store tests
// ========================================================================

#[test]
fn test_set_and_get_runtime() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    // Store an Arc<String> as a stand-in for Arc<dyn Source>
    let runtime: Arc<String> = Arc::new("test-runtime".to_string());
    graph
        .set_runtime("source-1", Box::new(runtime.clone()))
        .unwrap();

    // Retrieve it
    let retrieved = graph.get_runtime::<Arc<String>>("source-1");
    assert!(retrieved.is_some());
    assert_eq!(**retrieved.unwrap(), "test-runtime");
}

#[test]
fn test_get_runtime_wrong_type_returns_none() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    let runtime: Arc<String> = Arc::new("test".to_string());
    graph.set_runtime("source-1", Box::new(runtime)).unwrap();

    // Wrong type returns None
    let retrieved = graph.get_runtime::<Arc<i32>>("source-1");
    assert!(retrieved.is_none());
}

#[test]
fn test_get_runtime_nonexistent_returns_none() {
    let graph = create_test_graph();
    let retrieved = graph.get_runtime::<Arc<String>>("nonexistent");
    assert!(retrieved.is_none());
}

#[test]
fn test_take_runtime() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    let runtime: Arc<String> = Arc::new("take-me".to_string());
    graph.set_runtime("source-1", Box::new(runtime)).unwrap();

    // Take removes it
    let taken = graph.take_runtime::<Arc<String>>("source-1");
    assert!(taken.is_some());
    assert_eq!(*taken.unwrap(), "take-me");

    // Now it's gone
    assert!(!graph.has_runtime("source-1"));
    assert!(graph.get_runtime::<Arc<String>>("source-1").is_none());
}

#[test]
fn test_has_runtime() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    assert!(!graph.has_runtime("source-1"));

    graph.set_runtime("source-1", Box::new(42i32)).unwrap();
    assert!(graph.has_runtime("source-1"));
}

#[test]
fn test_remove_component_removes_runtime() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    graph
        .set_runtime("source-1", Box::new(Arc::new("runtime".to_string())))
        .unwrap();
    assert!(graph.has_runtime("source-1"));

    graph.remove_component("source-1").unwrap();
    assert!(!graph.has_runtime("source-1"));
}

#[test]
fn test_deregister_removes_runtime() {
    let mut graph = create_test_graph();
    graph.register_source("source-1", HashMap::new()).unwrap();

    graph
        .set_runtime("source-1", Box::new(Arc::new("runtime".to_string())))
        .unwrap();
    assert!(graph.has_runtime("source-1"));

    graph.deregister("source-1").unwrap();
    assert!(!graph.has_runtime("source-1"));
}

#[test]
fn test_set_runtime_replaces_existing() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    graph
        .set_runtime("source-1", Box::new(Arc::new("first".to_string())))
        .unwrap();
    graph
        .set_runtime("source-1", Box::new(Arc::new("second".to_string())))
        .unwrap();

    let retrieved = graph.get_runtime::<Arc<String>>("source-1");
    assert_eq!(**retrieved.unwrap(), "second");
}

// ========================================================================
// ComponentStatusHandle tests
// ========================================================================

#[tokio::test]
async fn test_status_handle_new_defaults_to_stopped() {
    let handle = ComponentStatusHandle::new("comp-1");
    assert_eq!(handle.get_status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_status_handle_set_and_get() {
    let handle = ComponentStatusHandle::new("comp-1");
    handle.set_status(ComponentStatus::Running, None).await;
    assert_eq!(handle.get_status().await, ComponentStatus::Running);
}

#[tokio::test]
async fn test_status_handle_new_wired_sends_update() {
    let (tx, mut rx) = mpsc::channel::<ComponentUpdate>(16);
    let handle = ComponentStatusHandle::new_wired("comp-1", tx);

    // Local status starts at Stopped
    assert_eq!(handle.get_status().await, ComponentStatus::Stopped);

    // set_status should update locally AND send via the channel
    handle
        .set_status(ComponentStatus::Running, Some("started".into()))
        .await;
    assert_eq!(handle.get_status().await, ComponentStatus::Running);

    let update = rx.try_recv().unwrap();
    match update {
        ComponentUpdate::Status {
            component_id,
            status,
            message,
        } => {
            assert_eq!(component_id, "comp-1");
            assert_eq!(status, ComponentStatus::Running);
            assert_eq!(message, Some("started".into()));
        }
    }
}

#[tokio::test]
async fn test_status_handle_unwired_does_not_send() {
    let handle = ComponentStatusHandle::new("comp-1");
    // No channel wired — set_status should still work locally
    handle.set_status(ComponentStatus::Error, None).await;
    assert_eq!(handle.get_status().await, ComponentStatus::Error);
}

#[tokio::test]
async fn test_status_handle_wire_after_creation() {
    let (tx, mut rx) = mpsc::channel::<ComponentUpdate>(16);
    let handle = ComponentStatusHandle::new("comp-1");

    // Wire later
    handle.wire(tx).await;

    handle.set_status(ComponentStatus::Starting, None).await;

    let update = rx.try_recv().unwrap();
    match update {
        ComponentUpdate::Status {
            component_id,
            status,
            ..
        } => {
            assert_eq!(component_id, "comp-1");
            assert_eq!(status, ComponentStatus::Starting);
        }
    }
}

#[tokio::test]
async fn test_status_handle_wire_only_first_call_takes_effect() {
    let (tx1, mut rx1) = mpsc::channel::<ComponentUpdate>(16);
    let (tx2, mut rx2) = mpsc::channel::<ComponentUpdate>(16);
    let handle = ComponentStatusHandle::new("comp-1");

    handle.wire(tx1).await;
    // Second wire is ignored (OnceCell)
    handle.wire(tx2).await;

    handle.set_status(ComponentStatus::Running, None).await;

    // Should arrive on first channel only
    assert!(rx1.try_recv().is_ok());
    assert!(rx2.try_recv().is_err());
}

#[tokio::test]
async fn test_status_handle_clone_shares_state() {
    let handle1 = ComponentStatusHandle::new("comp-1");
    let handle2 = handle1.clone();

    handle1.set_status(ComponentStatus::Running, None).await;
    assert_eq!(handle2.get_status().await, ComponentStatus::Running);
}

// ========================================================================
// subscribe() tests
// ========================================================================

#[test]
fn test_subscribe_receives_add_event() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    let mut event_rx = graph.subscribe();

    graph.add_component(source_node("source-1")).unwrap();

    let event = event_rx.try_recv().unwrap();
    assert_eq!(event.component_id, "source-1");
    assert_eq!(event.component_type, ComponentType::Source);
    assert_eq!(event.status, ComponentStatus::Stopped);
}

#[test]
fn test_subscribe_receives_status_change_event() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    let mut event_rx = graph.subscribe();

    graph.add_component(source_node("source-1")).unwrap();
    let _add_event = event_rx.try_recv().unwrap(); // consume add event

    // Transition Stopped → Starting
    graph
        .validate_and_transition("source-1", ComponentStatus::Starting, None)
        .unwrap();

    let event = event_rx.try_recv().unwrap();
    assert_eq!(event.component_id, "source-1");
    assert_eq!(event.status, ComponentStatus::Starting);
}

#[test]
fn test_subscribe_multiple_receivers() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    let mut rx1 = graph.subscribe();
    let mut rx2 = graph.subscribe();

    graph.add_component(source_node("source-1")).unwrap();

    assert_eq!(rx1.try_recv().unwrap().component_id, "source-1");
    assert_eq!(rx2.try_recv().unwrap().component_id, "source-1");
}

// ========================================================================
// apply_update() tests
// ========================================================================

#[test]
fn test_apply_update_changes_status() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();

    // Stopped → Starting is valid
    let event = graph.apply_update(ComponentUpdate::Status {
        component_id: "source-1".into(),
        status: ComponentStatus::Starting,
        message: Some("booting".into()),
    });

    assert!(event.is_some());
    let event = event.unwrap();
    assert_eq!(event.component_id, "source-1");
    assert_eq!(event.status, ComponentStatus::Starting);
    assert_eq!(event.message, Some("booting".into()));

    // Verify status was updated in the graph
    let node = graph.get_component("source-1").unwrap();
    assert_eq!(node.status, ComponentStatus::Starting);
}

#[test]
fn test_apply_update_emits_broadcast_event() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();
    let mut event_rx = graph.subscribe();
    let _add = event_rx.try_recv(); // consume add event

    graph.apply_update(ComponentUpdate::Status {
        component_id: "source-1".into(),
        status: ComponentStatus::Starting,
        message: None,
    });

    let event = event_rx.try_recv().unwrap();
    assert_eq!(event.component_id, "source-1");
    assert_eq!(event.status, ComponentStatus::Starting);
}

#[test]
fn test_apply_update_nonexistent_component_returns_none() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    let event = graph.apply_update(ComponentUpdate::Status {
        component_id: "nonexistent".into(),
        status: ComponentStatus::Running,
        message: None,
    });

    assert!(event.is_none());
}

#[test]
fn test_apply_update_invalid_transition_returns_none() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();

    // Stopped → Running is not a valid transition (must go through Starting)
    let event = graph.apply_update(ComponentUpdate::Status {
        component_id: "source-1".into(),
        status: ComponentStatus::Running,
        message: None,
    });

    assert!(event.is_none());
    // Status should remain Stopped
    assert_eq!(
        graph.get_component("source-1").unwrap().status,
        ComponentStatus::Stopped
    );
}

#[test]
fn test_apply_update_same_status_is_noop() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();

    let event = graph.apply_update(ComponentUpdate::Status {
        component_id: "source-1".into(),
        status: ComponentStatus::Stopped,
        message: None,
    });

    assert!(event.is_none());
}

// ========================================================================
// get_dependents() / get_dependencies() tests
// ========================================================================

#[test]
fn test_get_dependents_returns_empty_for_leaf() {
    let mut graph = create_test_graph();
    graph.add_component(reaction_node("reaction-1")).unwrap();

    assert!(graph.get_dependents("reaction-1").is_empty());
}

#[test]
fn test_get_dependents_returns_empty_for_nonexistent() {
    let graph = create_test_graph();
    assert!(graph.get_dependents("nonexistent").is_empty());
}

#[test]
fn test_get_dependents_multiple() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph.add_component(query_node("query-2")).unwrap();

    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    graph
        .add_relationship("source-1", "query-2", RelationshipKind::Feeds)
        .unwrap();

    let dependents = graph.get_dependents("source-1");
    assert_eq!(dependents.len(), 2);
    let ids: Vec<&str> = dependents.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&"query-1"));
    assert!(ids.contains(&"query-2"));
}

#[test]
fn test_get_dependencies_returns_empty_for_root_component() {
    let mut graph = create_test_graph();
    graph.add_component(source_node("source-1")).unwrap();

    // Source has no SubscribesTo edges
    assert!(graph.get_dependencies("source-1").is_empty());
}

#[test]
fn test_get_dependencies_returns_empty_for_nonexistent() {
    let graph = create_test_graph();
    assert!(graph.get_dependencies("nonexistent").is_empty());
}

#[test]
fn test_get_dependencies_follows_subscribes_to() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();

    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();

    // query-1 subscribes to source-1
    let deps = graph.get_dependencies("query-1");
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].id, "source-1");
}

#[test]
fn test_get_dependencies_multiple() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(source_node("source-2")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();

    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    graph
        .add_relationship("source-2", "query-1", RelationshipKind::Feeds)
        .unwrap();

    let deps = graph.get_dependencies("query-1");
    assert_eq!(deps.len(), 2);
    let ids: Vec<&str> = deps.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&"source-1"));
    assert!(ids.contains(&"source-2"));
}

#[test]
fn test_get_dependencies_chain() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();
    graph.add_component(query_node("query-1")).unwrap();
    graph.add_component(reaction_node("reaction-1")).unwrap();

    graph
        .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
        .unwrap();
    graph
        .add_relationship("query-1", "reaction-1", RelationshipKind::Feeds)
        .unwrap();

    // reaction-1 depends on query-1 (not transitively on source-1)
    let deps = graph.get_dependencies("reaction-1");
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].id, "query-1");
}

// ========================================================================
// wait_for_status() tests
// ========================================================================

#[tokio::test]
async fn test_wait_for_status_already_reached() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    let graph = Arc::new(RwLock::new(graph));

    {
        let mut g = graph.write().await;
        g.add_component(source_node("source-1")).unwrap();
    }

    // source-1 starts as Stopped, so waiting for Stopped should return immediately
    let result = wait_for_status(
        &graph,
        "source-1",
        &[ComponentStatus::Stopped],
        std::time::Duration::from_millis(100),
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_wait_for_status_component_not_found() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    let graph = Arc::new(RwLock::new(graph));

    let result = wait_for_status(
        &graph,
        "nonexistent",
        &[ComponentStatus::Running],
        std::time::Duration::from_millis(100),
    )
    .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[tokio::test]
async fn test_wait_for_status_timeout() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    let graph = Arc::new(RwLock::new(graph));

    {
        let mut g = graph.write().await;
        g.add_component(source_node("source-1")).unwrap();
    }

    // source-1 is Stopped, wait for Running with a short timeout
    let result = wait_for_status(
        &graph,
        "source-1",
        &[ComponentStatus::Running],
        std::time::Duration::from_millis(50),
    )
    .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Timed out"));
}

#[tokio::test]
async fn test_wait_for_status_reaches_target_via_update() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    let graph = Arc::new(RwLock::new(graph));

    {
        let mut g = graph.write().await;
        g.add_component(source_node("source-1")).unwrap();
    }

    let graph_clone = Arc::clone(&graph);
    // Spawn a task that transitions the component after a short delay
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut g = graph_clone.write().await;
        g.apply_update(ComponentUpdate::Status {
            component_id: "source-1".into(),
            status: ComponentStatus::Starting,
            message: None,
        });
    });

    let result = wait_for_status(
        &graph,
        "source-1",
        &[ComponentStatus::Starting],
        std::time::Duration::from_secs(2),
    )
    .await;

    handle.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), ComponentStatus::Starting);
}

#[tokio::test]
async fn test_wait_for_status_multiple_targets() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    let graph = Arc::new(RwLock::new(graph));

    {
        let mut g = graph.write().await;
        g.add_component(source_node("source-1")).unwrap();
    }

    // Stopped matches the second target
    let result = wait_for_status(
        &graph,
        "source-1",
        &[ComponentStatus::Running, ComponentStatus::Stopped],
        std::time::Duration::from_millis(100),
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), ComponentStatus::Stopped);
}

// ========================================================================
// GraphTransaction additional tests
// ========================================================================

#[test]
fn test_transaction_rollback_cleans_edges_and_nodes() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    // Pre-add source outside transaction
    graph.add_component(source_node("source-1")).unwrap();
    let initial_edge_count = graph.edge_count(); // 2 (Owns + OwnedBy)

    {
        let mut txn = graph.begin();
        txn.add_component(query_node("query-1")).unwrap();
        txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        // Drop without commit
    }

    assert!(!graph.contains("query-1"));
    assert_eq!(graph.edge_count(), initial_edge_count);
    assert!(graph.get_dependents("source-1").is_empty());
}

#[test]
fn test_transaction_commit_events_have_correct_status() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    let mut event_rx = graph.subscribe();

    {
        let mut txn = graph.begin();
        txn.add_component(source_node("source-1")).unwrap();
        txn.commit();
    }

    let event = event_rx.try_recv().unwrap();
    assert_eq!(event.status, ComponentStatus::Stopped);
    assert_eq!(event.component_type, ComponentType::Source);
}

#[test]
fn test_transaction_no_events_before_commit() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    let mut event_rx = graph.subscribe();

    {
        let mut txn = graph.begin();
        txn.add_component(source_node("source-1")).unwrap();
        txn.add_component(query_node("query-1")).unwrap();
        // Not committed yet — no events emitted
        assert!(event_rx.try_recv().is_err());
        txn.commit();
    }

    // Now events should be available
    assert!(event_rx.try_recv().is_ok());
    assert!(event_rx.try_recv().is_ok());
}

#[test]
fn test_transaction_add_relationship_between_existing_and_new() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();

    {
        let mut txn = graph.begin();
        txn.add_component(query_node("query-1")).unwrap();
        txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        txn.commit();
    }

    assert!(graph.contains("query-1"));
    assert_eq!(graph.get_dependents("source-1").len(), 1);
}

// ========================================================================
// record_event() / get_events() / get_all_events() / get_last_error()
// ========================================================================

#[test]
fn test_record_and_get_events() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    let event = ComponentEvent {
        component_id: "source-1".into(),
        component_type: ComponentType::Source,
        status: ComponentStatus::Starting,
        timestamp: chrono::Utc::now(),
        message: None,
    };
    graph.record_event(event.clone());

    let events = graph.get_events("source-1");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].component_id, "source-1");
    assert_eq!(events[0].status, ComponentStatus::Starting);
}

#[test]
fn test_get_events_empty_for_unknown_component() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    assert!(graph.get_events("nonexistent").is_empty());
}

#[test]
fn test_get_all_events_across_components() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    let event1 = ComponentEvent {
        component_id: "source-1".into(),
        component_type: ComponentType::Source,
        status: ComponentStatus::Starting,
        timestamp: chrono::Utc::now(),
        message: None,
    };
    let event2 = ComponentEvent {
        component_id: "query-1".into(),
        component_type: ComponentType::Query,
        status: ComponentStatus::Running,
        timestamp: chrono::Utc::now(),
        message: None,
    };
    graph.record_event(event1);
    graph.record_event(event2);

    let all = graph.get_all_events();
    assert_eq!(all.len(), 2);
}

#[test]
fn test_get_last_error_returns_none_when_no_errors() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    assert!(graph.get_last_error("source-1").is_none());
}

#[test]
fn test_get_last_error_returns_error_message() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    let event = ComponentEvent {
        component_id: "source-1".into(),
        component_type: ComponentType::Source,
        status: ComponentStatus::Error,
        timestamp: chrono::Utc::now(),
        message: Some("connection refused".into()),
    };
    graph.record_event(event);

    let error = graph.get_last_error("source-1");
    assert!(error.is_some());
    assert_eq!(error.unwrap(), "connection refused");
}

#[test]
fn test_apply_update_records_event_in_history() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();

    graph.apply_update(ComponentUpdate::Status {
        component_id: "source-1".into(),
        status: ComponentStatus::Starting,
        message: Some("boot".into()),
    });

    let events = graph.get_events("source-1");
    // add_component also records an event, plus the apply_update
    assert!(!events.is_empty());
    let last = events.last().unwrap();
    assert_eq!(last.status, ComponentStatus::Starting);
    assert_eq!(last.message, Some("boot".into()));
}

// ========================================================================
// subscribe_events() tests
// ========================================================================

#[test]
fn test_subscribe_events_returns_history_and_receiver() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    // Record some history first
    let event = ComponentEvent {
        component_id: "source-1".into(),
        component_type: ComponentType::Source,
        status: ComponentStatus::Starting,
        timestamp: chrono::Utc::now(),
        message: None,
    };
    graph.record_event(event);

    let (history, _rx) = graph.subscribe_events("source-1");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, ComponentStatus::Starting);
}

#[test]
fn test_subscribe_events_receives_new_events() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");
    graph.add_component(source_node("source-1")).unwrap();

    let (_history, mut event_rx) = graph.subscribe_events("source-1");

    // Record a new event after subscribing
    let event = ComponentEvent {
        component_id: "source-1".into(),
        component_type: ComponentType::Source,
        status: ComponentStatus::Running,
        timestamp: chrono::Utc::now(),
        message: None,
    };
    graph.record_event(event);

    let received = event_rx.try_recv().unwrap();
    assert_eq!(received.status, ComponentStatus::Running);
}

#[test]
fn test_subscribe_events_empty_history_for_new_component() {
    let (mut graph, _rx) = ComponentGraph::new("test-instance");

    let (history, _rx) = graph.subscribe_events("brand-new");
    assert!(history.is_empty());
}

// ========================================================================
// event_sender() / update_sender() / status_notifier() accessor tests
// ========================================================================

#[test]
fn test_event_sender_broadcasts_to_subscribers() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    let sender = graph.event_sender().clone();
    let mut rx = graph.subscribe();

    let event = ComponentEvent {
        component_id: "test".into(),
        component_type: ComponentType::Source,
        status: ComponentStatus::Running,
        timestamp: chrono::Utc::now(),
        message: None,
    };
    sender.send(event).unwrap();

    let received = rx.try_recv().unwrap();
    assert_eq!(received.component_id, "test");
}

#[tokio::test]
async fn test_update_sender_delivers_to_receiver() {
    let (graph, mut update_rx) = ComponentGraph::new("test-instance");
    let update_tx = graph.update_sender();

    update_tx
        .send(ComponentUpdate::Status {
            component_id: "comp-1".into(),
            status: ComponentStatus::Running,
            message: None,
        })
        .await
        .unwrap();

    let update = update_rx.recv().await.unwrap();
    match update {
        ComponentUpdate::Status {
            component_id,
            status,
            ..
        } => {
            assert_eq!(component_id, "comp-1");
            assert_eq!(status, ComponentStatus::Running);
        }
    }
}

#[test]
fn test_status_notifier_returns_arc() {
    let (graph, _rx) = ComponentGraph::new("test-instance");
    let n1 = graph.status_notifier();
    let n2 = graph.status_notifier();
    // Both should point to the same Notify
    assert!(Arc::ptr_eq(&n1, &n2));
}
