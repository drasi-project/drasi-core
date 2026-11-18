// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Insert-or-update decisions, cardinality caps, TTL scheduling and undo.

use super::*;
use crate::projection::tests::{
    client_span, gauge, metrics, point, resource, settings, traces, RECEIVED,
};
use crate::projection::{map_metrics, map_traces};
use drasi_core_types::models::ElementValue;
use serde_json::json;

const NS: &str = "6a1d2c3b-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

fn node(id: &str, category: ElementCategory, ttl: Option<u64>, group: u32) -> ProjectedElement {
    let mut properties = ElementPropertyMap::new();
    properties.insert("lastSeen", ElementValue::String(Arc::from("t")));
    ProjectedElement {
        id: id.into(),
        labels: vec![format!("{category:?}")],
        properties,
        kind: ProjectedKind::Node,
        effective_from: 1,
        category,
        ttl_secs: ttl,
        group,
    }
}

fn admitted(outcome: GroupOutcome) -> (Vec<SourceChange>, GroupUndo) {
    match outcome {
        GroupOutcome::Admitted(changes, undo) => (changes, undo),
        GroupOutcome::Dropped => panic!("group was dropped"),
    }
}

fn kinds(changes: &[SourceChange]) -> Vec<&'static str> {
    changes
        .iter()
        .map(|change| match change {
            SourceChange::Insert { .. } => "insert",
            SourceChange::Update { .. } => "update",
            SourceChange::Delete { .. } => "delete",
            _ => "other",
        })
        .collect()
}

#[test]
fn elements_split_into_groups_in_first_appearance_order() {
    let elements = vec![
        node("a", ElementCategory::Service, None, 4),
        node("b", ElementCategory::Metric, None, 2),
        node("c", ElementCategory::Reports, None, 4),
    ];
    let split = groups(elements);
    let ids: Vec<Vec<&str>> = split
        .iter()
        .map(|group| group.iter().map(|element| element.id.as_str()).collect())
        .collect();
    assert_eq!(ids, vec![vec!["a", "c"], vec!["b"]]);
}

#[test]
fn a_first_observation_inserts_and_a_later_one_updates() {
    let config = settings(json!({}));
    let mut state = ProjectionState::default();
    let (first, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        RECEIVED,
    ));
    assert_eq!(kinds(&first), vec!["insert"]);
    let (second, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        RECEIVED,
    ));
    assert_eq!(kinds(&second), vec!["update"]);
    assert_eq!(state.count(ElementCategory::Service), 1);
    assert_eq!(state.len(), 1);
    // Changes carry the component namespace.
    assert_eq!(first[0].get_reference().namespace.as_ref(), NS);
}

#[test]
fn services_are_stamped_registered_at_only_on_insert() {
    let config = settings(json!({}));
    let mut state = ProjectionState::default();
    let (insert, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        RECEIVED,
    ));
    let (update, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        RECEIVED,
    ));
    let registered = |change: &SourceChange| match change {
        SourceChange::Insert { element } | SourceChange::Update { element, .. } => {
            element.get_properties().get("registeredAt").cloned()
        }
        _ => None,
    };
    assert_eq!(
        registered(&insert[0]),
        Some(ElementValue::String(Arc::from("t")))
    );
    // The update leaves the inserted registeredAt in place downstream.
    assert_eq!(registered(&update[0]), None);
    let (metric, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("m", ElementCategory::Metric, None, 0)],
        RECEIVED,
    ));
    assert_eq!(registered(&metric[0]), None);
}

#[test]
fn a_group_with_a_new_element_over_its_cap_is_dropped_whole() {
    let config = settings(json!({"maxServices": 1, "maxMetrics": 1}));
    let mut state = ProjectionState::default();
    admitted(state.apply_group(
        NS,
        &config,
        vec![
            node("svc:a", ElementCategory::Service, None, 0),
            node("metric:a", ElementCategory::Metric, None, 0),
            node("reports:a", ElementCategory::Reports, None, 0),
        ],
        RECEIVED,
    ));
    // A new service with its metric: the service is over cap, so the metric
    // and relation are not admitted either.
    let dropped = state.apply_group(
        NS,
        &config,
        vec![
            node("svc:b", ElementCategory::Service, None, 0),
            node("metric:b", ElementCategory::Metric, None, 0),
        ],
        RECEIVED,
    );
    assert!(matches!(dropped, GroupOutcome::Dropped));
    assert!(!state.contains("svc:b") && !state.contains("metric:b"));
    // Existing elements still update at the cap.
    let (changes, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![
            node("svc:a", ElementCategory::Service, None, 0),
            node("metric:a", ElementCategory::Metric, None, 0),
        ],
        RECEIVED,
    ));
    assert_eq!(kinds(&changes), vec!["update", "update"]);
    // Uncapped categories are never limited.
    let (changes, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![
            node("svc:a", ElementCategory::Service, None, 0),
            node("hb:a", ElementCategory::Heartbeat, None, 0),
            node("rel-hb:a", ElementCategory::HeartbeatRel, None, 0),
        ],
        RECEIVED,
    ));
    assert_eq!(kinds(&changes), vec!["update", "insert", "insert"]);
}

#[test]
fn two_new_elements_in_one_group_count_against_the_cap_together() {
    let config = settings(json!({"maxServices": 2}));
    let mut state = ProjectionState::default();
    admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        RECEIVED,
    ));
    let over = state.apply_group(
        NS,
        &config,
        vec![
            node("svc:b", ElementCategory::Service, None, 0),
            node("svc:c", ElementCategory::Service, None, 0),
        ],
        RECEIVED,
    );
    assert!(matches!(over, GroupOutcome::Dropped));
    // One new id repeated within a group counts once.
    let (changes, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![
            node("svc:b", ElementCategory::Service, None, 0),
            node("svc:b", ElementCategory::Service, None, 0),
        ],
        RECEIVED,
    ));
    assert_eq!(kinds(&changes), vec!["insert", "update"]);
    assert_eq!(state.count(ElementCategory::Service), 2);
}

#[test]
fn undo_restores_ids_counts_and_previous_ttls() {
    let config = settings(json!({}));
    let mut state = ProjectionState::default();
    admitted(state.apply_group(
        NS,
        &config,
        vec![node("dep:a", ElementCategory::DependsOn, Some(10), 0)],
        1_000,
    ));
    let (_, undo) = admitted(state.apply_group(
        NS,
        &config,
        vec![
            node("dep:a", ElementCategory::DependsOn, Some(10), 0),
            node("dep:b", ElementCategory::DependsOn, Some(10), 0),
        ],
        5_000,
    ));
    state.undo(undo);
    assert!(state.contains("dep:a"));
    assert!(!state.contains("dep:b"));
    assert_eq!(state.count(ElementCategory::DependsOn), 1);
    // dep:a keeps its original expiry, not the refreshed one.
    let due: Vec<(String, u64)> = state
        .due(11_000)
        .into_iter()
        .map(|record| (record.id, record.expires_at))
        .collect();
    assert_eq!(due, vec![("dep:a".to_owned(), 11_000)]);
}

#[test]
fn ttls_expire_from_receipt_and_refreshing_postpones_them() {
    let config = settings(json!({}));
    let mut state = ProjectionState::default();
    admitted(state.apply_group(
        NS,
        &config,
        vec![node("dep:a", ElementCategory::DependsOn, Some(10), 0)],
        1_000,
    ));
    admitted(state.apply_group(
        NS,
        &config,
        vec![node("log:b", ElementCategory::LogEvent, Some(5), 0)],
        1_000,
    ));
    assert!(state.due(5_999).is_empty());
    let due = state.due(6_000);
    assert_eq!(due.len(), 1);
    assert_eq!(
        delete_change(NS, &due[0], 6_000)
            .get_reference()
            .element_id
            .as_ref(),
        "log:b"
    );
    state.expire(&due);
    assert!(!state.contains("log:b"));
    assert_eq!(state.count(ElementCategory::LogEvent), 0);
    // Refreshing dep:a at 9s moves its expiry to 19s.
    let (refresh, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("dep:a", ElementCategory::DependsOn, Some(10), 0)],
        9_000,
    ));
    assert_eq!(kinds(&refresh), vec!["update"]);
    assert!(state.due(18_999).is_empty());
    let due = state.due(19_000);
    assert_eq!(due.len(), 1);
    state.expire(&due);
    assert!(state.is_empty());
    // An expired id is new again.
    let (again, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("dep:a", ElementCategory::DependsOn, Some(10), 0)],
        20_000,
    ));
    assert_eq!(kinds(&again), vec!["insert"]);
}

#[test]
fn expiring_a_record_refreshed_since_it_was_read_keeps_the_element() {
    let config = settings(json!({}));
    let mut state = ProjectionState::default();
    admitted(state.apply_group(
        NS,
        &config,
        vec![node("dep:a", ElementCategory::DependsOn, Some(1), 0)],
        0,
    ));
    let stale = state.due(1_000);
    admitted(state.apply_group(
        NS,
        &config,
        vec![node("dep:a", ElementCategory::DependsOn, Some(1), 0)],
        1_000,
    ));
    state.expire(&stale);
    assert!(state.contains("dep:a"));
    assert_eq!(state.due(2_000).len(), 1);
}

#[test]
fn expiry_deletes_carry_labels_and_the_expiry_time() {
    let record = TtlRecord {
        id: "dep:a".into(),
        labels: vec!["DEPENDS_ON".into()],
        expires_at: 10,
    };
    match delete_change(NS, &record, 42) {
        SourceChange::Delete { metadata } => {
            assert_eq!(metadata.effective_from, 42);
            assert_eq!(metadata.labels.as_ref(), &[Arc::<str>::from("DEPENDS_ON")]);
            assert_eq!(metadata.reference, ElementReference::new(NS, "dep:a"));
        }
        other => panic!("expected a delete, got {other:?}"),
    }
}

#[test]
fn projected_relations_reference_their_endpoints_in_the_namespace() {
    let config = settings(json!({}));
    let out = map_traces(
        &traces(
            resource(&[("service.name", "checkout")]),
            vec![client_span(&[("peer.service", "payments")], 0)],
        ),
        &config,
        RECEIVED,
    );
    let mut state = ProjectionState::default();
    let mut changes = Vec::new();
    for group in groups(out.elements) {
        changes.extend(admitted(state.apply_group(NS, &config, group, RECEIVED)).0);
    }
    assert_eq!(kinds(&changes), vec!["insert", "insert", "insert"]);
    match &changes[2] {
        SourceChange::Insert {
            element: Element::Relation {
                in_node, out_node, ..
            },
        } => {
            assert_eq!(in_node, &ElementReference::new(NS, "svc:checkout"));
            assert_eq!(out_node, &ElementReference::new(NS, "svc:payments"));
        }
        other => panic!("expected a relation insert, got {other:?}"),
    }
}

#[test]
fn repeated_exports_of_one_series_update_the_same_metric() {
    let config = settings(json!({"metricAllowlist": ["latency"]}));
    let mut state = ProjectionState::default();
    let mut all = Vec::new();
    for value in [920.0, 700.0] {
        let request = metrics(
            resource(&[("service.name", "checkout")]),
            vec![gauge("latency", vec![point(value, 0, &[])])],
        );
        for group in groups(map_metrics(&request, &config, RECEIVED).elements) {
            all.push(kinds(
                &admitted(state.apply_group(NS, &config, group, RECEIVED)).0,
            ));
        }
    }
    assert_eq!(
        all,
        vec![
            vec!["insert", "insert", "insert"],
            vec!["update", "update", "update"]
        ]
    );
    assert_eq!(state.count(ElementCategory::Metric), 1);
}

#[test]
fn reconciling_discarded_changes_follows_the_first_change_per_element() {
    let config = settings(json!({}));
    let mut state = ProjectionState::default();
    // svc:a was delivered earlier; svc:b's insert and svc:a's update were
    // both discarded, then svc:a expired with its delete discarded too.
    admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        1_000,
    ));
    let (b_insert, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:b", ElementCategory::Service, None, 0)],
        1_000,
    ));
    let (a_update, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        1_000,
    ));
    let record = TtlRecord {
        id: "svc:a".into(),
        labels: vec!["Service".into()],
        expires_at: 2_000,
    };
    state.ttl_by_id.insert(record.id.clone(), record.expires_at);
    state.ttls.entry(2_000).or_default().push(record.clone());
    state.expire(std::slice::from_ref(&record));
    let a_delete = delete_change(NS, &record, 2_000);
    let discarded: Vec<_> = b_insert
        .into_iter()
        .chain(a_update)
        .chain([a_delete])
        .collect();

    assert_eq!(state.reconcile_discarded(&discarded), 2);
    assert!(state.is_empty());
    assert_eq!(state.count(ElementCategory::Service), 0);
    let due = state.due(0);
    assert_eq!(
        due.len(),
        1,
        "the graph still has svc:a, so it is deleted again"
    );
    assert_eq!(due[0].id, "svc:a");

    // Observing svc:a again before the sweep inserts it and cancels the
    // repeated delete.
    let (changes, _) = admitted(state.apply_group(
        NS,
        &config,
        vec![node("svc:a", ElementCategory::Service, None, 0)],
        3_000,
    ));
    assert_eq!(kinds(&changes), vec!["insert"]);
    assert!(state.due(u64::MAX).is_empty());
}
