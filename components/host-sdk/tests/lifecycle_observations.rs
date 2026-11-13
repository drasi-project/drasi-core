// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use drasi_host_sdk::callbacks::{instance_lifecycle_callback, InstanceCallbackContext};
use drasi_lib::{
    observability::get_or_init_global_registry,
    reactions::common::base::{ReactionBase, ReactionBaseParams},
    ComponentStatus, DrasiLib, Query, Reaction, ReactionRuntimeContext,
};
use drasi_plugin_sdk::ffi::{FfiLifecycleEvent, FfiLifecycleEventType, FfiStr};

struct CallbackReaction {
    base: ReactionBase,
    callback: Mutex<Option<Arc<InstanceCallbackContext>>>,
}

#[async_trait]
impl Reaction for CallbackReaction {
    fn id(&self) -> &drasi_core_types::ComponentId {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "callback-test"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_inputs(&self) -> Vec<drasi_computation_contracts::ComponentReference> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        false
    }
    async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
        *self.callback.lock().expect("callback ownership") =
            Some(Arc::new(InstanceCallbackContext {
                instance_id: context.instance_id.clone(),
                runtime_handle: tokio::runtime::Handle::current(),
                log_registry: get_or_init_global_registry(),
                update_tx: context.update_tx.clone(),
            }));
        self.base.bind_runtime_context(context).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        {
            let context = self
                .callback
                .lock()
                .expect("callback ownership")
                .clone()
                .expect("initialized callback")
                .into_raw();
            let emit = |event_type| {
                instance_lifecycle_callback(
                    context,
                    &FfiLifecycleEvent {
                        component_id: FfiStr::from_str(self.id().as_str()),
                        component_type: FfiStr::from_str("reaction"),
                        event_type,
                        message: FfiStr::from_str("failure after saturated FFI callback"),
                        timestamp_us: 0,
                    },
                );
            };
            for _ in 0..10_000 {
                emit(FfiLifecycleEventType::Starting);
            }
            emit(FfiLifecycleEventType::Started);
            emit(FfiLifecycleEventType::Error);
            emit(FfiLifecycleEventType::Stopped);
            // Callbacks borrow the context synchronously and never retain this pointer.
            drop(unsafe { Arc::from_raw(context as *const InstanceCallbackContext) });
        }
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}

async fn callback_flood(instance: &str) {
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id(instance)
        .with_query(
            Query::cypher(
                drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            )
            .query("MATCH (n:Person) RETURN n.name AS name")
            .auto_start(false)
            .enable_bootstrap(false)
            .build()
            .expect("query configuration"),
        )
        .with_reaction(
            drasi_core_types::ComponentName::try_new("callback").expect("fixture component name"),
            CallbackReaction {
                base: ReactionBase::new(ReactionBaseParams::new(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000122")
                        .expect("fixture UUID"),
                    vec![drasi_computation_contracts::ComponentReference::by_name(
                        drasi_core_types::ComponentName::try_new("query")
                            .expect("fixture component name"),
                    )],
                )),
                callback: Mutex::new(None),
            },
        )
        .build()
        .await
        .expect("instance");
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .expect("start query");
    let error = core
        .start_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("callback").expect("fixture component name"),
        )
        .await
        .expect_err("the callback failure must prevent successful activation");
    assert!(
        format!("{error:#}").contains("failure after saturated FFI callback"),
        "{error:#}"
    );
    core.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn ffi_callback_catches_up_after_saturation_current_thread() {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        callback_flood("callback-current"),
    )
    .await
    .expect("current-thread callback observation stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ffi_callback_catches_up_after_saturation_multi_thread() {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        callback_flood("callback-multi"),
    )
    .await
    .expect("multi-thread callback observation stalled");
}
