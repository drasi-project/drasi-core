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

#![cfg(feature = "computation")]
#[allow(dead_code)]
mod computation_support;
use computation_support::*;
use drasi_lib::{computation::v1::*, DrasiLib};
use std::{
    collections::BTreeSet,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

fn graph() -> ComputationGraph {
    ComputationGraph::builder("inspected")
        .source(Box::new(FiniteSource::new("source", vec![])))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            Arc::new(Mutex::new(Vec::new())),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            edge("source", "sink"),
            RelationshipPolicy {
                required_for_binding: false,
                orphan_permitted: true,
                ..Default::default()
            },
        )
        .build()
        .expect("graph")
}

#[tokio::test]
async fn inspection_history_is_coherent_and_exposed_without_legacy_graph_nodes() {
    let drasi = DrasiLib::builder().build().await.expect("instance");
    let handle = drasi
        .add_computation_graph(graph(), ComputationOptions { auto_start: false })
        .await
        .expect("register");
    handle.deployment().await.expect("deploy");
    let inspector = drasi
        .inspect_computation_graph("inspected")
        .await
        .expect("inspection");
    let before = inspector.snapshot().sequence;
    let control = handle.control();
    control
        .set_lifecycle_policy(
            GraphRevision(1),
            component("sink"),
            LifecyclePolicy { auto_start: false },
        )
        .await
        .expect("desired update");
    let snapshots = inspector.history(before).expect("history");
    assert!(!snapshots.is_empty());
    for snapshot in snapshots {
        assert_eq!(snapshot.desired.revision, snapshot.observed.revision);
    }
    assert_eq!(inspector.snapshot().desired.revision, GraphRevision(2));
    assert!(drasi
        .component_graph()
        .read()
        .await
        .get_component("sink")
        .is_none());
    drasi.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn computation_topology_is_queryable_as_typed_graph_changes() {
    let target = graph();
    let mut source = ComputationTopologySource::new(
        component("topology"),
        stream("topology"),
        target.inspector(),
    );
    let mut query = ContinuousQueryTransformer::new(ContinuousQueryDefinition {
        graph_id: "inspection-query".into(), id: component("query"),
        query: "MATCH (g:ComputationGraph)-[:HAS_COMPONENT]->(c:ComputationComponent) RETURN c.id AS id".into(),
        language: ComputationQueryLanguage::Cypher, output_stream: stream("query"),
        outbox_capacity: NonZeroUsize::new(16).expect("capacity"),
    }, Arc::new(drasi_core::computation::InMemoryComputationProvider)).await.expect("query");
    query.start().await.expect("start query");
    source.start().await.expect("start topology source");
    for _ in 0..6 {
        let event = source
            .next()
            .await
            .expect("next")
            .expect("initial topology");
        query
            .transform(InputEnvelope {
                port: port("in"),
                envelope: event.envelope,
            })
            .await
            .expect("query topology");
    }
    let ids: BTreeSet<_> = query
        .results()
        .snapshot()
        .expect("snapshot")
        .rows
        .values()
        .map(|row| QueryChangeCodec::decode_row(row).expect("row").values["id"].to_string())
        .collect();
    assert_eq!(
        ids,
        BTreeSet::from(["source".to_string(), "sink".to_string()])
    );
    source.stop().await.expect("stop source");
    query.stop().await.expect("stop query");
}

#[tokio::test]
async fn orphan_relationships_remain_queryable_and_history_eviction_is_explicit() {
    let drasi = DrasiLib::builder().build().await.expect("instance");
    let handle = drasi
        .add_computation_graph(graph(), ComputationOptions { auto_start: false })
        .await
        .expect("register");
    handle.deployment().await.expect("deployment");
    let control = handle.control();
    let preview = control
        .preview(
            GraphRevision(1),
            vec![DesiredMutation::Unbind {
                edge: edge("source", "sink"),
                policy: RemovalPolicy::Orphan,
            }],
        )
        .await
        .expect("preview");
    control
        .reconcile(preview, TopologyBindings::default())
        .await
        .expect("unbind");
    let inspector = handle.inspector();
    let mut source = ComputationTopologySource::new(
        component("topology"),
        stream("topology"),
        inspector.clone(),
    );
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "orphan-inspection".into(),
            id: component("query"),
            query: "MATCH (r:ComputationRelationship) RETURN r.binding AS binding".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("query"),
            outbox_capacity: NonZeroUsize::new(16).expect("capacity"),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .expect("query");
    query.start().await.expect("query start");
    source.start().await.expect("topology start");
    for _ in 0..9 {
        let envelope = source
            .next()
            .await
            .expect("event")
            .expect("topology")
            .envelope;
        query
            .transform(InputEnvelope {
                port: port("in"),
                envelope,
            })
            .await
            .expect("projection");
    }
    let rows = query.results().snapshot().expect("snapshot").rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        QueryChangeCodec::decode_row(rows.values().next().expect("row"))
            .expect("decode")
            .values["binding"]
            .to_string(),
        "Declared"
    );
    for index in 0..270 {
        let revision = control.desired_snapshot().revision;
        control
            .set_lifecycle_policy(
                revision,
                component("sink"),
                LifecyclePolicy {
                    auto_start: index % 2 == 0,
                },
            )
            .await
            .expect("policy");
    }
    assert!(
        inspector.history(0).is_err(),
        "evicted history must not look complete"
    );
    source.stop().await.expect("topology stop");
    query.stop().await.expect("query stop");
    drasi.shutdown().await.expect("shutdown");
}
