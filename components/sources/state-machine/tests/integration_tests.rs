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

//! End-to-end tests for the state machine source.
//!
//! These wire a real `DrasiLib` instance: an application source feeds order data
//! through three stage queries into the state machine source, which transitions
//! entities and dispatches their state as node changes. A downstream query over
//! the state machine source is observed via an application reaction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use drasi_source_state_machine::config::{EnterCondition, Op, StateDef};
use drasi_source_state_machine::{
    StateMachineSource, StateMachineSourceBuilder, StateMachineSourceConfig,
};
use serde_json::Value;
use tokio::sync::Mutex;

/// Shared record of the latest observed `state` per `orderId`, plus the full
/// ordered history of states seen.
#[derive(Default)]
struct Observed {
    latest: HashMap<String, String>,
    history: Vec<(String, String)>,
}

type ObservedHandle = Arc<Mutex<Observed>>;

fn order_states() -> Vec<StateDef> {
    vec![
        StateDef {
            id: "NEW".to_string(),
            enter: vec![EnterCondition {
                query: "draft-orders".to_string(),
                previous: vec![],
                key: "{{orderId}}".to_string(),
                ops: vec![Op::Added],
            }],
        },
        StateDef {
            id: "CONFIRMED".to_string(),
            enter: vec![EnterCondition {
                query: "confirmed-orders".to_string(),
                previous: vec!["NEW".to_string()],
                key: "{{orderId}}".to_string(),
                ops: vec![Op::Added],
            }],
        },
        StateDef {
            id: "PAID".to_string(),
            enter: vec![EnterCondition {
                query: "paid-orders".to_string(),
                previous: vec!["CONFIRMED".to_string()],
                key: "{{orderId}}".to_string(),
                ops: vec![Op::Added],
            }],
        },
    ]
}

fn state_machine_config() -> StateMachineSourceConfig {
    StateMachineSourceConfig {
        entity_label: "OrderState".to_string(),
        key_field: "orderId".to_string(),
        states: order_states(),
    }
}

fn stage_queries() -> Vec<drasi_lib::config::QueryConfig> {
    vec![
        Query::cypher("draft-orders")
            .query("MATCH (o:Order) WHERE o.is_draft = true RETURN o.id AS orderId")
            .from_source("orders")
            .auto_start(true)
            .build(),
        Query::cypher("confirmed-orders")
            .query("MATCH (o:Order) WHERE o.is_draft = false RETURN o.id AS orderId")
            .from_source("orders")
            .auto_start(true)
            .build(),
        Query::cypher("paid-orders")
            .query("MATCH (o:Order) WHERE o.paid = true RETURN o.id AS orderId")
            .from_source("orders")
            .auto_start(true)
            .build(),
    ]
}

/// Build the downstream query over the state source and an application reaction
/// that records observed `(orderId, state)` pairs into `observed`.
async fn downstream_recorder(
    source_id: &str,
    observed: ObservedHandle,
) -> (drasi_lib::config::QueryConfig, ApplicationReaction) {
    let query = Query::cypher("order-states")
        .query("MATCH (o:OrderState) RETURN o.orderId AS orderId, o.state AS state")
        .from_source(source_id)
        .auto_start(true)
        .build();

    let (reaction, handle) =
        ApplicationReaction::new("state-observer", vec!["order-states".into()]);

    handle
        .subscribe(move |result| {
            let observed = observed.clone();
            // The callback is sync; spawn to record asynchronously.
            tokio::spawn(async move {
                let mut guard = observed.lock().await;
                for diff in &result.results {
                    let row = match diff {
                        drasi_lib::channels::ResultDiff::Add { data, .. } => Some(data.clone()),
                        drasi_lib::channels::ResultDiff::Update { after, .. } => {
                            Some(after.clone())
                        }
                        _ => None,
                    };
                    if let Some(Value::Object(map)) = row {
                        if let (Some(Value::String(id)), Some(Value::String(state))) =
                            (map.get("orderId"), map.get("state"))
                        {
                            guard.latest.insert(id.clone(), state.clone());
                            guard.history.push((id.clone(), state.clone()));
                        }
                    }
                }
            });
        })
        .await
        .expect("subscribe to order-states");

    (query, reaction)
}

async fn wait_for_state(observed: &ObservedHandle, order_id: &str, want: &str) -> bool {
    for _ in 0..100 {
        if observed
            .lock()
            .await
            .latest
            .get(order_id)
            .map(|s| s == want)
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn state_machine_progresses_and_exposes_state_source() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        drasi_state_store_redb::RedbStateStoreProvider::new(tmp.path().join("state.redb")).unwrap(),
    );
    let observed: ObservedHandle = Arc::new(Mutex::new(Observed::default()));

    let (source, handle) = ApplicationSource::new(
        "orders",
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )
    .unwrap();

    let sm_source_id = "order-state-source-progress";
    let sm_source = StateMachineSource::new(sm_source_id, state_machine_config()).unwrap();

    let (downstream_query, observer) = downstream_recorder(sm_source_id, observed.clone()).await;

    let mut builder = DrasiLib::builder()
        .with_id("state-machine-test")
        .with_state_store_provider(store.clone())
        .with_source(source)
        .with_source(sm_source)
        .with_reaction(observer)
        .with_query(downstream_query);
    for q in stage_queries() {
        builder = builder.with_query(q);
    }
    let core = Arc::new(builder.build().await.unwrap());
    core.start().await.unwrap();

    // Draft order -> NEW
    handle
        .send_node_insert(
            "o1",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("id", "o1")
                .with_bool("is_draft", true)
                .with_bool("paid", false)
                .build(),
        )
        .await
        .unwrap();
    assert!(
        wait_for_state(&observed, "o1", "NEW").await,
        "order should enter NEW"
    );

    // Confirm -> CONFIRMED
    handle
        .send_node_update(
            "o1",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("id", "o1")
                .with_bool("is_draft", false)
                .with_bool("paid", false)
                .build(),
        )
        .await
        .unwrap();
    assert!(
        wait_for_state(&observed, "o1", "CONFIRMED").await,
        "order should enter CONFIRMED"
    );

    // Pay -> PAID
    handle
        .send_node_update(
            "o1",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("id", "o1")
                .with_bool("is_draft", false)
                .with_bool("paid", true)
                .build(),
        )
        .await
        .unwrap();
    assert!(
        wait_for_state(&observed, "o1", "PAID").await,
        "order should enter PAID"
    );

    // Verify the progression order was observed.
    let history: Vec<String> = observed
        .lock()
        .await
        .history
        .iter()
        .filter(|(id, _)| id == "o1")
        .map(|(_, s)| s.clone())
        .collect();
    assert!(history.contains(&"NEW".to_string()));
    assert!(history.contains(&"CONFIRMED".to_string()));
    assert!(history.contains(&"PAID".to_string()));

    // Verify state persisted under the source partition.
    let persisted = store.list_keys(sm_source_id).await.unwrap();
    assert!(persisted.contains(&"o1".to_string()));

    core.stop().await.unwrap();
}

#[tokio::test]
async fn fresh_subscriber_bootstraps_from_persisted_state() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        drasi_state_store_redb::RedbStateStoreProvider::new(tmp.path().join("state.redb")).unwrap(),
    );
    let source_id = "order-state-source-bootstrap";

    // Pre-seed the state store with a PAID order as a prior run would have done.
    let record = serde_json::json!({
        "key": "o9",
        "key_field": "orderId",
        "label": "OrderState",
        "state": "PAID",
        "previous_state": "CONFIRMED",
        "entered_at": 1_700_000_000_000_i64,
        "fields": { "orderId": "o9" }
    });
    store
        .set(source_id, "o9", serde_json::to_vec(&record).unwrap())
        .await
        .unwrap();

    let sm_source = StateMachineSource::new(source_id, state_machine_config()).unwrap();
    let downstream_query = Query::cypher("order-states")
        .query("MATCH (o:OrderState) RETURN o.orderId AS orderId, o.state AS state")
        .from_source(source_id)
        .auto_start(true)
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("state-machine-bootstrap-test")
            .with_state_store_provider(store.clone())
            .with_source(sm_source)
            .with_query(downstream_query)
            .build()
            .await
            .unwrap(),
    );
    core.start().await.unwrap();

    // The downstream query should bootstrap the persisted PAID order from the source.
    let mut found = false;
    for _ in 0..100 {
        if let Ok(results) = core.get_query_results("order-states").await {
            if results.iter().any(|row| {
                row.get("orderId").and_then(|v| v.as_str()) == Some("o9")
                    && row.get("state").and_then(|v| v.as_str()) == Some("PAID")
            }) {
                found = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found,
        "fresh subscriber should bootstrap the PAID order from persisted state"
    );

    core.stop().await.unwrap();
}

/// Regression test for the robust query-consumption engine: a query result emitted
/// *before* the source subscribes must still be processed via **outbox catch-up**.
///
/// A bare live-forwarding source (the earlier design) would miss it. The shared
/// consumption engine replays the query's outbox from the checkpoint on fresh
/// start, so the source catches up.
#[tokio::test]
async fn source_catches_up_on_results_emitted_before_it_subscribes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        drasi_state_store_redb::RedbStateStoreProvider::new(tmp.path().join("state.redb")).unwrap(),
    );

    let (source, handle) = ApplicationSource::new(
        "orders",
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )
    .unwrap();

    // Start with ONLY the application source + stage queries — no state machine
    // source yet, so the draft-orders result lands in the query outbox before any
    // state machine subscription exists.
    let mut builder = DrasiLib::builder()
        .with_id("sm-catchup-test")
        .with_state_store_provider(store.clone())
        .with_source(source);
    for q in stage_queries() {
        builder = builder.with_query(q);
    }
    let core = Arc::new(builder.build().await.unwrap());
    core.start().await.unwrap();

    // Emit a draft order BEFORE the state machine source exists/subscribes.
    handle
        .send_node_insert(
            "o1",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("id", "o1")
                .with_bool("is_draft", true)
                .with_bool("paid", false)
                .build(),
        )
        .await
        .unwrap();

    // Wait until draft-orders has processed it (it is now in the query's outbox).
    let mut in_query = false;
    for _ in 0..100 {
        if let Ok(results) = core.get_query_results("draft-orders").await {
            if results
                .iter()
                .any(|r| r.get("orderId").and_then(|v| v.as_str()) == Some("o1"))
            {
                in_query = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        in_query,
        "draft-orders should have processed o1 before the source subscribes"
    );

    // NOW add the state machine source + a downstream query and start the source.
    // Add it with auto_start=false so wiring (and thus catch-up) happens
    // deterministically when we call start_source below.
    let sm_source = StateMachineSourceBuilder::new("order-state-source")
        .with_config(state_machine_config())
        .with_auto_start(false)
        .build()
        .unwrap();
    core.add_source(sm_source).await.unwrap();
    core.add_query(
        Query::cypher("order-states")
            .query("MATCH (o:OrderState) RETURN o.orderId AS orderId, o.state AS state")
            .from_source("order-state-source")
            .auto_start(true)
            .build(),
    )
    .await
    .unwrap();
    core.start_source("order-state-source").await.unwrap();

    // The source must catch up on the draft-orders result emitted before it
    // subscribed, transitioning o1 to NEW. A live-only forwarder would miss it.
    let mut found = false;
    for _ in 0..100 {
        if let Ok(results) = core.get_query_results("order-states").await {
            if results.iter().any(|r| {
                r.get("orderId").and_then(|v| v.as_str()) == Some("o1")
                    && r.get("state").and_then(|v| v.as_str()) == Some("NEW")
            }) {
                found = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found,
        "state machine source should catch up on the pre-subscription draft-orders \
         result and set o1=NEW"
    );

    core.stop().await.unwrap();
}
