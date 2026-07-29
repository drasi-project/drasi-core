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

//! End-to-end test for the source query-subscription FFI path.
//!
//! Loads the `drasi-source-state-machine` crate as a **dynamic cdylib plugin**
//! and drives a full `DrasiLib` flow through it:
//!
//! ```text
//! application source ──▶ stage queries ──▶ (dynamic) state machine source ──▶ downstream query ──▶ application reaction
//! ```
//!
//! This exercises the vtable additions (`subscribed_query_ids_fn` +
//! `start_result_push_fn`) and the `SourceProxy` push bridge: query results are
//! forwarded across the FFI boundary into the plugin's `enqueue_query_result`,
//! the plugin computes state transitions, and re-emits entity-state nodes that a
//! downstream query observes.
//!
//! **Prerequisite**: build the plugin cdylib first:
//!
//! ```sh
//! cargo build --lib -p drasi-source-state-machine --features drasi-source-state-machine/dynamic-plugin
//! # or: cargo xtask build-plugins
//! ```
//!
//! The test SKIPs (panics with a SKIP message) when the cdylib is not present.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::load_plugin_from_path;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, Query, Source};
use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use serde_json::Value;
use tokio::sync::Mutex;

// ----------------------------------------------------------------------------
// Plugin discovery helpers (mirrors integration_test.rs)
// ----------------------------------------------------------------------------

fn plugin_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("Cannot find workspace root from CARGO_MANIFEST_DIR");
    workspace_root.join("target").join("debug").join("plugins")
}

fn plugin_filename(crate_name: &str) -> String {
    let lib_name = crate_name.replace('-', "_");
    if cfg!(target_os = "macos") {
        format!("lib{lib_name}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{lib_name}.dll")
    } else {
        format!("lib{lib_name}.so")
    }
}

fn plugin_path(crate_name: &str) -> PathBuf {
    plugin_dir().join(plugin_filename(crate_name))
}

fn plugin_exists(crate_name: &str) -> bool {
    plugin_path(crate_name).exists()
}

// ----------------------------------------------------------------------------
// Observation helper
// ----------------------------------------------------------------------------

#[derive(Default)]
struct Observed {
    latest: HashMap<String, String>,
    history: Vec<(String, String)>,
}

type ObservedHandle = Arc<Mutex<Observed>>;

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

/// The state machine config, exactly as it would appear in YAML/JSON. Note the
/// three referenced queries — the dynamically-loaded source must report these via
/// the FFI `subscribed_query_ids_fn`.
fn state_machine_config_json() -> Value {
    serde_json::json!({
        "entityLabel": "OrderState",
        "keyField": "orderId",
        "states": [
            {
                "id": "NEW",
                "enter": [
                    { "query": "draft-orders", "previous": [], "key": "{{orderId}}", "ops": ["added"] }
                ]
            },
            {
                "id": "CONFIRMED",
                "enter": [
                    { "query": "confirmed-orders", "previous": ["NEW"], "key": "{{orderId}}", "ops": ["added"] }
                ]
            },
            {
                "id": "PAID",
                "enter": [
                    { "query": "paid-orders", "previous": ["CONFIRMED"], "key": "{{orderId}}", "ops": ["added"] }
                ]
            }
        ]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_state_machine_source_drives_transitions_over_ffi() {
    if !plugin_exists("drasi-source-state-machine") {
        eprintln!(
            "SKIP: drasi-source-state-machine not built as cdylib. Build with: \
             cargo build --lib -p drasi-source-state-machine \
             --features drasi-source-state-machine/dynamic-plugin"
        );
        panic!("SKIP: drasi-source-state-machine not built as cdylib");
    }

    // ------------------------------------------------------------------
    // 1. Load the state machine source as a dynamic cdylib plugin.
    // ------------------------------------------------------------------
    let path = plugin_path("drasi-source-state-machine");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("load state machine source plugin");

    assert_eq!(
        plugin.source_plugins.len(),
        1,
        "state machine plugin should export exactly one source descriptor"
    );
    let descriptor = &plugin.source_plugins[0];
    assert_eq!(descriptor.kind(), "state-machine");

    // Create the source instance over FFI from the config.
    let sm_source = descriptor
        .create_source("order-state-source", &state_machine_config_json(), true)
        .await
        .expect("create dynamic state machine source");
    assert_eq!(sm_source.id(), "order-state-source");
    // The proxy must report the referenced queries across FFI.
    let mut subs = sm_source.subscribed_query_ids();
    subs.sort();
    assert_eq!(
        subs,
        vec![
            "confirmed-orders".to_string(),
            "draft-orders".to_string(),
            "paid-orders".to_string()
        ],
        "dynamic source must expose its subscribed queries via FFI"
    );

    // ------------------------------------------------------------------
    // 2. Assemble a DrasiLib around the dynamic source.
    // ------------------------------------------------------------------
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

    let downstream_query = Query::cypher("order-states")
        .query("MATCH (o:OrderState) RETURN o.orderId AS orderId, o.state AS state")
        .from_source("order-state-source")
        .auto_start(true)
        .build();

    let (observer, obs_handle) =
        ApplicationReaction::new("state-observer", vec!["order-states".into()]);
    {
        let observed = observed.clone();
        obs_handle
            .subscribe(move |result| {
                let observed = observed.clone();
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
    }

    let mut builder = DrasiLib::builder()
        .with_id("dynamic-state-machine-test")
        .with_state_store_provider(store.clone())
        .with_source(source)
        .with_source(sm_source) // the dynamically-loaded Box<dyn Source>
        .with_reaction(observer)
        .with_query(downstream_query);
    for q in stage_queries() {
        builder = builder.with_query(q);
    }
    let core = Arc::new(builder.build().await.unwrap());
    core.start().await.unwrap();

    // ------------------------------------------------------------------
    // 3. Drive an order through the lifecycle and assert transitions.
    // ------------------------------------------------------------------
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
        "order should enter NEW via the dynamic source"
    );

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
        "order should enter CONFIRMED via the dynamic source"
    );

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
        "order should enter PAID via the dynamic source"
    );

    // The dynamic source persisted state through its FFI state-store bridge.
    let persisted = store.list_keys("order-state-source").await.unwrap();
    assert!(
        persisted.contains(&"o1".to_string()),
        "dynamic source should persist entity state"
    );

    core.stop().await.unwrap();
    // Keep the loaded plugin alive until the end of the test.
    drop(plugin);
}
