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

#![cfg(all(feature = "computation", feature = "middleware-relabel"))]
use drasi_lib::{
    computation::v1::*,
    config::{QueryJoinConfig, QueryJoinKeyConfig},
    ComponentStatus, DrasiLib,
};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::time::Duration;

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
    let pipeline = drasi.computation_pipeline("native").expect("pipeline");
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
    let desired = graph
        .snapshot()
        .select(GraphSelection::All)
        .expect("desired export");
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
    let native = drasi
        .add_computation_graph(graph, ComputationOptions::default())
        .await
        .expect("register graph");
    drasi.start().await.expect("start both paths");
    assert_eq!(
        native
            .control()
            .startup_report()
            .await
            .expect("startup")
            .summary,
        OperationSummary::Completed
    );
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
    assert!(
        services
            .state_store
            .as_ref()
            .expect("scoped state")
            .get("native-output", "checkpoint:joined")
            .await
            .expect("checkpoint lookup")
            .is_none(),
        "accepted application deliveries must not create handled checkpoints"
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
    assert!(drasi
        .component_graph()
        .read()
        .await
        .get_component(native_source.as_str())
        .is_none());
    native.stop().await.expect("stop new pipeline only");
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
    native.start().await.expect("restart native pipeline");
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
    drasi
        .remove_computation_graph("native")
        .await
        .expect("remove only native pipeline");
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
async fn same_instance_existing_plugins_join_middleware_and_restart_current_thread() {
    tokio::time::timeout(Duration::from_secs(20), scenario())
        .await
        .expect("pipeline deadlock");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_instance_existing_plugins_join_middleware_and_restart_multi_thread() {
    tokio::time::timeout(Duration::from_secs(20), scenario())
        .await
        .expect("pipeline deadlock");
}
