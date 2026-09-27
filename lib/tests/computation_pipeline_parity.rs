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

//! Ordinary Source/Reaction adapters and explicitly assembled components retain
//! identical joins, middleware, output identities, and independent lifecycles.

#![cfg(test)]
#![cfg(feature = "middleware-relabel")]
use drasi_lib::{
    computation::v1::*,
    config::{QueryJoinConfig, QueryJoinKeyConfig},
    ComponentStatus, DrasiLib,
};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::time::Duration;

#[tokio::test]
async fn separately_assembled_pipelines_share_one_root_without_helper_collisions() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let (source, input) = ApplicationSource::new(
            "input",
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )
        .unwrap();
        let query = |id| {
            drasi_lib::Query::cypher(id)
                .query("MATCH (n:Item) RETURN n.value AS value")
                .from_source("input")
                .enable_bootstrap(false)
                .build()
        };
        let core = DrasiLib::builder()
            .with_id("shared-root-pipelines")
            .with_source(source)
            .with_query(query("ordinary"))
            .build()
            .await
            .unwrap();
        core.computation_component("ordinary")
            .unwrap()
            .wait_created()
            .await
            .unwrap();
        let query_graph = core.inspect_query_computation("ordinary").await.unwrap();
        let before = query_graph.snapshot();
        let mut readers = Vec::new();
        for id in ["first", "second"] {
            let pipeline = core.computation_pipeline().unwrap();
            readers.push((id, pipeline.catalog()));
            let batch = pipeline
                .source(
                    core.borrow_computation_source("input").await.unwrap(),
                    SourceSubscriptionOptions::default(),
                )
                .unwrap()
                .query(query(id))
                .build()
                .unwrap();
            assert!(core.add_components(batch).await.unwrap().committed);
        }
        let inventory = core.inspect_computation_inventory().await.unwrap();
        assert_eq!(
            inventory
                .scopes
                .values()
                .filter(|scope| scope.owner.is_none())
                .count(),
            1
        );
        assert_eq!(inventory.scopes.len(), 2);
        let after = query_graph.snapshot();
        assert_eq!(after.desired.id, before.desired.id);
        assert_eq!(after.desired.revision, before.desired.revision);
        for (id, state) in &before.observed.components {
            assert_eq!(after.observed.components[id].generation, state.generation);
        }
        core.start().await.unwrap();
        input
            .send_node_insert(
                "one",
                vec!["Item"],
                PropertyMapBuilder::new().with_integer("value", 42).build(),
            )
            .await
            .unwrap();
        for (id, catalog) in readers {
            loop {
                let snapshot = catalog.snapshot(id, Duration::from_secs(5)).await.unwrap();
                if snapshot.as_of_sequence == 1 {
                    assert_eq!(snapshot.rows.len(), 1);
                    let row = QueryChangeCodec::decode_row(snapshot.rows.values().next().unwrap())
                        .unwrap();
                    assert_eq!(
                        QueryChangeCodec::row_values_to_json(&row.values),
                        serde_json::json!({"value":42})
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        loop {
            let rows = core.get_query_results("ordinary").await.unwrap();
            if !rows.is_empty() {
                assert_eq!(rows, vec![serde_json::json!({"value":42})]);
                break;
            }
            tokio::task::yield_now().await;
        }
        core.shutdown().await.unwrap();
    })
    .await
    .expect("independent pipeline additions must complete");
}

async fn scenario() {
    let (orders, orders_input) = ApplicationSource::new(
        "orders",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )
    .expect("orders");
    let (customers, customers_input) = ApplicationSource::new(
        "customers",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )
    .expect("customers");
    let config = drasi_lib::Query::cypher("joined")
        .query(
            "MATCH (o:Order)-[:CUSTOMER]->(c:Customer) RETURN o.name AS item, c.name AS customer",
        )
        .from_source_with_pipeline("orders", vec!["normalize".into()])
        .from_source("customers")
        .with_middleware(drasi_core::models::SourceMiddlewareConfig::new(
            "relabel",
            "normalize",
            serde_json::json!({"labelMappings":{"RawOrder":"Order"}})
                .as_object()
                .expect("object")
                .clone(),
        ))
        .with_joins(vec![QueryJoinConfig {
            id: "CUSTOMER".into(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "Order".into(),
                    property: "customerId".into(),
                },
                QueryJoinKeyConfig {
                    label: "Customer".into(),
                    property: "id".into(),
                },
            ],
        }])
        .enable_bootstrap(false)
        .build();
    let (legacy_reaction, legacy_handle) =
        ApplicationReaction::new("legacy-output", vec!["joined".into()]);
    let mut legacy_output = legacy_handle
        .take_receiver()
        .await
        .expect("legacy receiver");
    let drasi = DrasiLib::builder()
        .with_id("parallel-instance")
        .with_source(orders)
        .with_source(customers)
        .with_query(config.clone())
        .with_reaction(legacy_reaction)
        .build()
        .await
        .expect("instance");
    for id in ["orders", "customers", "joined", "legacy-output"] {
        drasi
            .computation_component(id)
            .expect("ordinary component handle")
            .wait_created()
            .await
            .expect("ordinary component creation");
    }
    let native_core = DrasiLib::builder()
        .with_id("native-instance")
        .build()
        .await
        .expect("native instance");
    let pipeline = native_core.computation_pipeline().expect("pipeline");
    let catalog = pipeline.catalog();
    let services = pipeline.services();
    let (native_reaction, native_handle) =
        ApplicationReaction::new("native-output", vec!["joined".into()]);
    let mut native_output = native_handle
        .take_receiver()
        .await
        .expect("native receiver");
    let reaction = ReactionPluginHost::owned(
        Box::new(native_reaction),
        services.clone(),
        catalog.clone(),
        ReactionPluginOptions {
            bootstrap_timeout_secs: 5,
            ..Default::default()
        },
    )
    .expect("native reaction");
    let graph = pipeline
        .source(
            drasi
                .borrow_computation_source("orders")
                .await
                .expect("borrow orders"),
            SourceSubscriptionOptions::default(),
        )
        .expect("orders")
        .source(
            drasi
                .borrow_computation_source("customers")
                .await
                .expect("borrow customers"),
            SourceSubscriptionOptions::default(),
        )
        .expect("customers")
        .query(config)
        .reaction(reaction, true)
        .build()
        .expect("native graph");
    let desired = graph.definition.clone();
    assert!(
        desired
            .components
            .iter()
            .all(|component| matches!(component.construction, ComponentConstruction::Factory(_))),
        "compatibility pipelines, including their outlet, have reconstructable factories"
    );
    let imported = DesiredTopology::from_json(&desired.to_json().expect("desired JSON"))
        .expect("parse desired");
    let mut unbound = imported
        .build(TopologyBindings {
            factories: FactoryRegistry::standard(),
            ..Default::default()
        })
        .expect("all built-in compatibility factories resolve on import");
    assert_eq!(
        unbound.snapshot().specifications.len(),
        desired.components.len()
    );
    unbound
        .dispose()
        .await
        .expect("unbound desired graph cleanup");
    native_core
        .add_components(graph)
        .await
        .expect("register graph");
    drasi
        .start()
        .await
        .expect("start both graph construction paths");
    native_core.start().await.expect("start native instance");
    let native = native_core
        .computation_control()
        .expect("native controller");
    for id in ["orders", "customers", "joined", "legacy-output"] {
        drasi
            .computation_component(id)
            .expect("ordinary component handle")
            .wait_started()
            .await
            .expect("ordinary component readiness");
    }
    assert_eq!(
        native.startup_report().await.expect("startup").summary,
        OperationSummary::Completed
    );
    let initial_checkpoint = services
        .state_store
        .as_ref()
        .expect("scoped state")
        .get("native-output", "checkpoint:joined")
        .await
        .expect("checkpoint lookup")
        .expect("fresh-start cutoff");
    let cutoff: drasi_lib::ReactionCheckpoint =
        bincode::deserialize(&initial_checkpoint).expect("checkpoint payload");
    assert_eq!(cutoff.sequence, 0);
    customers_input
        .send_node_insert(
            "C1",
            vec!["Customer"],
            PropertyMapBuilder::new()
                .with_string("id", "C1")
                .with_string("name", "Ada")
                .build(),
        )
        .await
        .expect("customer");
    for sequence in 1..=2 {
        orders_input
            .send_node_insert(
                format!("O{sequence}"),
                vec!["RawOrder"],
                PropertyMapBuilder::new()
                    .with_string("customerId", "C1")
                    .with_string("name", format!("order-{sequence}"))
                    .build(),
            )
            .await
            .expect("order");
        let old = legacy_output.recv().await.expect("legacy result");
        let new = native_output.recv().await.expect("native result");
        assert_eq!(
            new.results, old.results,
            "same joins and middleware produce the same logical diffs and row identity"
        );
        assert_eq!(new.sequence, old.sequence);
    }
    assert_eq!(
        services
            .state_store
            .as_ref()
            .expect("scoped state")
            .get("native-output", "checkpoint:joined")
            .await
            .expect("checkpoint lookup")
            .expect("fresh-start cutoff remains"),
        initial_checkpoint,
        "accepted application deliveries must not advance the fresh-start cutoff"
    );
    assert_eq!(
        catalog
            .snapshot("joined", Duration::from_secs(5))
            .await
            .expect("native snapshot")
            .rows
            .len(),
        2
    );
    assert_eq!(
        drasi
            .get_query_results("joined")
            .await
            .expect("legacy snapshot")
            .len(),
        2
    );
    let native_source =
        ComputationPipelineBuilder::source_component_id("joined", "orders").expect("source ID");
    assert!(native
        .desired_snapshot()
        .nodes
        .iter()
        .any(|node| node.descriptor.id() == &native_source));
    native_core.stop().await.expect("stop native instance only");
    for source in ["orders", "customers"] {
        assert_eq!(
            drasi
                .get_source_status(source)
                .await
                .expect("legacy source status"),
            ComponentStatus::Running
        );
    }
    assert_eq!(
        drasi
            .get_query_status("joined")
            .await
            .expect("legacy query"),
        ComponentStatus::Running
    );
    assert_eq!(
        drasi
            .get_reaction_status("legacy-output")
            .await
            .expect("legacy reaction"),
        ComponentStatus::Running
    );
    native_core.start().await.expect("restart native instance");
    // ApplicationReaction only acknowledges queue acceptance; replay on restart
    // is intentionally at-least-once rather than a false handled checkpoint.
    for _ in 0..2 {
        native_output.recv().await.expect("native retained replay");
    }
    orders_input
        .send_node_insert(
            "O3",
            vec!["RawOrder"],
            PropertyMapBuilder::new()
                .with_string("customerId", "C1")
                .with_string("name", "order-3")
                .build(),
        )
        .await
        .expect("new order");
    assert_eq!(
        native_output
            .recv()
            .await
            .expect("native after restart")
            .results,
        legacy_output
            .recv()
            .await
            .expect("legacy remains live")
            .results
    );
    assert_eq!(
        catalog
            .snapshot("joined", Duration::from_secs(5))
            .await
            .expect("native snapshot")
            .rows
            .len(),
        3
    );
    native_core
        .shutdown()
        .await
        .expect("shutdown native instance");
    assert_eq!(
        drasi
            .get_source_status("orders")
            .await
            .expect("source remains owned by legacy"),
        ComponentStatus::Running
    );
    drasi.shutdown().await.expect("shutdown instance");
}

#[tokio::test(flavor = "current_thread")]
async fn existing_plugins_join_middleware_and_restart_current_thread() {
    tokio::time::timeout(Duration::from_secs(20), scenario())
        .await
        .expect("pipeline deadlock");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_plugins_join_middleware_and_restart_multi_thread() {
    tokio::time::timeout(Duration::from_secs(20), scenario())
        .await
        .expect("pipeline deadlock");
}
