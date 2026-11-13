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
    config::{ComponentReference, QueryJoinConfig, QueryJoinKeyConfig},
    ComponentStatus, DrasiLib,
};
use drasi_reaction_application::{ApplicationReaction, ApplicationReactionHandle};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use std::{sync::Arc, time::Duration};

fn name(value: &str) -> ComponentName {
    ComponentName::try_new(value).expect("fixture component name")
}

fn query_factory() -> Arc<dyn ComponentFactory> {
    FactoryRegistry::standard()
        .get(&ImplementationIdentity::try_new("drasi/continuous-query", "1").unwrap())
        .expect("reference query factory")
        .clone()
}

async fn source(core: &DrasiLib, source_name: &str) -> (ComponentHandle, ApplicationSourceHandle) {
    let (send, receive) = tokio::sync::oneshot::channel();
    let component = core
        .create_source(name(source_name), true, move |id| {
            let (source, input) = ApplicationSource::new(
                id,
                ApplicationSourceConfig {
                    properties: Default::default(),
                    durability: None,
                },
            )?;
            assert!(send.send(input).is_ok(), "source handle receiver");
            Ok(source)
        })
        .await
        .expect("admit source");
    component.wait_created().await.expect("source creation");
    (component, receive.await.expect("source handle"))
}

async fn reaction(core: &DrasiLib) -> (ComponentHandle, ApplicationReactionHandle) {
    let (send, receive) = tokio::sync::oneshot::channel();
    let component = core
        .create_reaction_with_inputs(
            name("legacy-output"),
            vec![ComponentReference::by_name(name("joined"))],
            true,
            move |id, inputs| {
                let (reaction, handle) = ApplicationReaction::new(
                    id,
                    inputs.into_iter().map(ComponentReference::by_id).collect(),
                );
                assert!(send.send(handle).is_ok(), "reaction handle receiver");
                Ok(reaction)
            },
        )
        .await
        .expect("admit reaction");
    component.wait_created().await.expect("reaction creation");
    (component, receive.await.expect("reaction handle"))
}

async fn settle(core: &DrasiLib) {
    let status = core.reconcile_desired_state().await.expect("reconcile");
    assert!(
        status.error.is_none() && status.resource_errors.is_empty(),
        "{status:?}"
    );
}

#[tokio::test]
async fn separately_assembled_pipelines_share_one_root_without_helper_collisions() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let query = |id| {
            drasi_lib::Query::cypher(name(id))
                .query("MATCH (n:Item) RETURN n.value AS value")
                .from_input_by_name(name("input"))
                .enable_bootstrap(false)
                .build()
                .unwrap()
        };
        let core = DrasiLib::builder()
            .with_query_factory(query_factory())
            .with_id("shared-root-pipelines")
            .build()
            .await
            .unwrap();
        let (source, input) = source(&core, "input").await;
        core.add_query_with_handle(query("ordinary"))
            .await
            .unwrap()
            .wait_created()
            .await
            .unwrap();
        let query_graph = core
            .inspect_query_computation_by_name(&name("ordinary"))
            .await
            .unwrap();
        let before = query_graph.snapshot();
        let mut readers = Vec::new();
        for id in ["first", "second"] {
            let pipeline = core.computation_pipeline().unwrap();
            let catalog = pipeline.catalog();
            let host = core
                .borrow_computation_source_by_id(source.id())
                .await
                .unwrap();
            let mut config = query(id);
            config.inputs[0].component = ComponentReference::by_id(source.id().clone());
            let receipt = core
                .create_components(move |ids| {
                    let query_id = ids.allocate()?;
                    pipeline
                        .source(host, SourceSubscriptionOptions::default())?
                        .query(config.with_assigned_id(query_id))
                        .build_owned(ids)
                })
                .await
                .unwrap();
            settle(&core).await;
            let assigned = receipt
                .creation
                .unwrap()
                .components
                .into_iter()
                .find(|component| component.name == name(id))
                .expect("admitted query");
            core.component_by_id(&assigned.id)
                .unwrap()
                .wait_created()
                .await
                .unwrap();
            readers.push((assigned.id, catalog));
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
        assert_eq!(inventory.scopes.len(), 1);
        let after = query_graph.snapshot();
        assert_eq!(after.desired.id, before.desired.id);
        assert!(after.desired.revision > before.desired.revision);
        for id in ["ordinary", "first", "second"] {
            assert!(after
                .observed
                .components
                .contains_key(core.component_by_name(&name(id)).unwrap().id()));
        }
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
                let snapshot = catalog.snapshot(&id, Duration::from_secs(5)).await.unwrap();
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
            let rows = core
                .get_query_results_by_name(&name("ordinary"))
                .await
                .unwrap();
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
    let drasi = DrasiLib::builder()
        .with_query_factory(query_factory())
        .with_id("parallel-instance")
        .build()
        .await
        .expect("instance");
    let (orders, orders_input) = source(&drasi, "orders").await;
    let (customers, customers_input) = source(&drasi, "customers").await;
    let mut config = drasi_lib::Query::cypher(name("joined"))
        .query(
            "MATCH (o:Order)-[:CUSTOMER]->(c:Customer) RETURN o.name AS item, c.name AS customer",
        )
        .from_input_with_pipeline_by_name(name("orders"), vec!["normalize".into()])
        .from_input_by_name(name("customers"))
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
        .build()
        .expect("query configuration");
    let joined = drasi
        .add_query_with_handle(config.clone())
        .await
        .expect("admit ordinary query");
    joined
        .wait_created()
        .await
        .expect("ordinary query creation");
    let (legacy_reaction, legacy_handle) = reaction(&drasi).await;
    let mut legacy_output = legacy_handle
        .take_receiver()
        .await
        .expect("legacy receiver");
    let native_core = DrasiLib::builder()
        .with_query_factory(query_factory())
        .with_id("native-instance")
        .build()
        .await
        .expect("native instance");
    let pipeline = native_core.computation_pipeline().expect("pipeline");
    let catalog = pipeline.catalog();
    let services = pipeline.services();
    let orders_host = drasi
        .borrow_computation_source_by_id(orders.id())
        .await
        .expect("borrow orders");
    let customers_host = drasi
        .borrow_computation_source_by_id(customers.id())
        .await
        .expect("borrow customers");
    config.inputs[0].component = ComponentReference::by_id(orders.id().clone());
    config.inputs[1].component = ComponentReference::by_id(customers.id().clone());
    let (send, receive) = tokio::sync::oneshot::channel();
    let reaction_services = services.clone();
    let reaction_catalog = catalog.clone();
    let receipt = native_core
        .create_components(move |ids| {
            let query_id = ids.allocate()?;
            let reaction_id = ids.allocate()?;
            let (native_reaction, native_handle) = ApplicationReaction::new(
                reaction_id,
                vec![ComponentReference::by_id(query_id.clone())],
            );
            let reaction = ReactionPluginHost::owned(
                Box::new(native_reaction),
                reaction_services,
                reaction_catalog,
                ReactionPluginOptions {
                    bootstrap_timeout_secs: 5,
                    ..Default::default()
                },
            )
            .expect("native reaction");
            let graph = pipeline
                .source(orders_host, SourceSubscriptionOptions::default())?
                .source(customers_host, SourceSubscriptionOptions::default())?
                .query(config.with_assigned_id(query_id))
                .reaction(name("native-output"), reaction, true)
                .build_owned(ids)?;
            assert!(
                send.send((native_handle, graph.definition.clone())).is_ok(),
                "native fixture receiver"
            );
            Ok(graph)
        })
        .await
        .expect("register native graph");
    let (native_handle, desired) = receive.await.expect("native fixtures");
    let mut native_output = native_handle
        .take_receiver()
        .await
        .expect("native receiver");
    settle(&native_core).await;
    let assignments = receipt.creation.expect("native assignments").components;
    let assigned = |component_name| {
        assignments
            .iter()
            .find(|component| component.name == name(component_name))
            .expect("admitted native component")
            .id
            .clone()
    };
    let native_query_id = assigned("joined");
    let native_reaction_id = assigned("native-output");
    native_core
        .component_by_id(&native_query_id)
        .unwrap()
        .wait_created()
        .await
        .expect("native query creation");
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
    drasi
        .start()
        .await
        .expect("start both graph construction paths");
    native_core.start().await.expect("start native instance");
    let native = native_core
        .computation_control()
        .expect("native controller");
    for id in [orders.id(), customers.id(), joined.id(), legacy_reaction.id()] {
        drasi
            .component_by_id(id)
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
        .get(
            &drasi_lib::StorePartition::component(native_reaction_id.clone()),
            &format!("checkpoint:{native_query_id}"),
        )
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
            .get(
                &drasi_lib::StorePartition::component(native_reaction_id),
                &format!("checkpoint:{native_query_id}"),
            )
            .await
            .expect("checkpoint lookup")
            .expect("fresh-start cutoff remains"),
        initial_checkpoint,
        "accepted application deliveries must not advance the fresh-start cutoff"
    );
    assert_eq!(
        catalog
            .snapshot(&native_query_id, Duration::from_secs(5))
            .await
            .expect("native snapshot")
            .rows
            .len(),
        2
    );
    assert_eq!(
        drasi
            .get_query_results_by_id(joined.id())
            .await
            .expect("legacy snapshot")
            .len(),
        2
    );
    let native_source =
        ComputationPipelineBuilder::source_component_id(&native_query_id, orders.id())
            .expect("source ID");
    assert!(native
        .desired_snapshot()
        .nodes
        .iter()
        .any(|node| node.descriptor.id() == &native_source));
    native_core.stop().await.expect("stop native instance only");
    for source in [orders.id(), customers.id()] {
        assert_eq!(
            drasi
                .get_source_status_by_id(source)
                .await
                .expect("legacy source status"),
            ComponentStatus::Running
        );
    }
    assert_eq!(
        drasi
            .get_query_status_by_id(joined.id())
            .await
            .expect("legacy query"),
        ComponentStatus::Running
    );
    assert_eq!(
        drasi
            .get_reaction_status_by_id(legacy_reaction.id())
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
            .snapshot(&native_query_id, Duration::from_secs(5))
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
            .get_source_status_by_id(orders.id())
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
