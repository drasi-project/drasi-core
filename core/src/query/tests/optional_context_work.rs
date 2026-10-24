// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{ElementIndex, ElementStream, IndexError},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, QueryJoin, SourceChange,
    },
    path_solver::match_path::MatchPath,
    query::QueryBuilder,
};

struct CountingIndex {
    inner: InMemoryElementIndex,
    scans: AtomicUsize,
}

#[async_trait]
impl ElementIndex for CountingIndex {
    async fn get_element(&self, id: &ElementReference) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_element(id).await
    }
    async fn set_element(&self, element: &Element, slots: &Vec<usize>) -> Result<(), IndexError> {
        self.inner.set_element(element, slots).await
    }
    async fn delete_element(&self, id: &ElementReference) -> Result<(), IndexError> {
        self.inner.delete_element(id).await
    }
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        id: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_slot_element_by_ref(slot, id).await
    }
    async fn get_slot_elements(&self, slot: usize) -> Result<ElementStream, IndexError> {
        self.scans.fetch_add(1, Ordering::Relaxed);
        self.inner.get_slot_elements(slot).await
    }
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        id: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner.get_slot_elements_by_inbound(slot, id).await
    }
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        id: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner.get_slot_elements_by_outbound(slot, id).await
    }
    async fn clear(&self) -> Result<(), IndexError> {
        ElementIndex::clear(&self.inner).await
    }
    async fn set_joins(&self, path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        self.inner.set_joins(path, joins).await;
    }
}

fn element(id: &str, label: &str, time: u64, properties: serde_json::Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("context-work", id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

#[tokio::test]
async fn isolated_context_updates_do_not_rescan_every_required_anchor() {
    const WORKLOADS: usize = 12;
    let index = Arc::new(CountingIndex {
        inner: InMemoryElementIndex::new(),
        scans: AtomicUsize::new(0),
    });
    let query = QueryBuilder::new(
        "MATCH (w:Workload) OPTIONAL MATCH (p:Plan) OPTIONAL MATCH (r:Readiness) RETURN w.id AS workload, p.version AS version, r.ready AS ready",
        Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new()))),
    ).with_element_index(index.clone()).build().await;
    for id in 0..WORKLOADS {
        query
            .process_source_change(SourceChange::Insert {
                element: element(
                    &format!("workload-{id}"),
                    "Workload",
                    1000,
                    json!({"id":id}),
                ),
            })
            .await
            .unwrap();
    }
    for node in [
        element("plan", "Plan", 1000, json!({"version":1})),
        element("ready", "Readiness", 1000, json!({"ready":false})),
    ] {
        query
            .process_source_change(SourceChange::Insert { element: node })
            .await
            .unwrap();
    }
    index.scans.store(0, Ordering::Relaxed);
    let changes = query
        .process_source_change(SourceChange::Update {
            element: element("ready", "Readiness", 2000, json!({"ready":true})),
        })
        .await
        .unwrap();
    assert_eq!(changes.len(), WORKLOADS);
    for change in changes {
        let QueryPartEvaluationContext::Updating { before, after, .. } = change else {
            panic!("context property update must preserve the MATCH row identity");
        };
        assert_eq!(serde_json::to_value(&before).unwrap()["ready"], false);
        assert_eq!(serde_json::to_value(&after).unwrap()["ready"], true);
        assert_eq!(before["workload"], after["workload"]);
        assert_eq!(before["version"], after["version"]);
    }
    let scans = index.scans.load(Ordering::Relaxed);
    assert!(
        scans <= 2 * (WORKLOADS + 1),
        "an unchanged isolated binding needs one before/after solve, not every required anchor: {scans} scans"
    );
    for (label, time, expected_before, expected_after) in [
        ("Other", 3000, json!(true), serde_json::Value::Null),
        ("Readiness", 4000, serde_json::Value::Null, json!(true)),
    ] {
        let changes = query
            .process_source_change(SourceChange::Update {
                element: element("ready", label, time, json!({"ready":true})),
            })
            .await
            .unwrap();
        assert_eq!(changes.len(), WORKLOADS * 2);
        let mut removed = 0;
        let mut added = 0;
        for change in changes {
            match change {
                QueryPartEvaluationContext::Removing { before, .. } => {
                    assert_eq!(
                        serde_json::to_value(before).unwrap()["ready"],
                        expected_before
                    );
                    removed += 1;
                }
                QueryPartEvaluationContext::Adding { after, .. } => {
                    assert_eq!(
                        serde_json::to_value(after).unwrap()["ready"],
                        expected_after
                    );
                    added += 1;
                }
                _ => panic!("changed slot membership must replace the old MATCH identity"),
            }
        }
        assert_eq!(removed, WORKLOADS);
        assert_eq!(added, WORKLOADS);
    }
}
