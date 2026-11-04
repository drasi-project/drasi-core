// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::{
    channels::QueryResult,
    observability::{
        MetricRegistrationContext, RootSampling, TelemetryOptions, TelemetryProfile,
        TelemetrySettings,
    },
    reactions::common::base::{ReactionBase, ReactionBaseParams},
    state_store::{StateStoreError, StateStoreProvider, StateStoreResult},
    ComponentStatus, DrasiLib, MemoryStateStoreProvider, Query, Reaction, ReactionRuntimeContext,
};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

#[path = "../../lib/tests/computation_support/metrics.rs"]
mod recording;
use recording::RecordingMetrics;

struct ControlledReaction {
    base: ReactionBase,
    entered: Arc<Notify>,
    permits: Arc<Semaphore>,
    fail_effect: bool,
}

#[async_trait]
impl Reaction for ControlledReaction {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "controlled-metrics"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        true
    }
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }
    async fn start(&self) -> Result<()> {
        let base = self.base.clone_shared();
        let entered = self.entered.clone();
        let permits = self.permits.clone();
        let fail_effect = self.fail_effect;
        let shutdown = self.base.create_shutdown_channel().await;
        drasi_lib::context::workers::spawn_owned_worker(&self.base.processing_task, async move {
            if let Err(error) = base
                .run_standard_loop(
                    shutdown,
                    HashMap::new(),
                    drasi_lib::ReactionRecoveryPolicy::Strict,
                    move |_| {
                        let entered = entered.clone();
                        let permits = permits.clone();
                        async move {
                            entered.notify_one();
                            permits.acquire().await?.forget();
                            anyhow::ensure!(!fail_effect, "controlled handler failure");
                            Ok(())
                        }
                    },
                )
                .await
            {
                tracing::error!("Controlled reaction failed: {error:#}");
            }
        })
        .await?;
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }
}

async fn ready(drasi: &DrasiLib, status: ComponentStatus) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = drasi.subscribe_all_component_events();
        loop {
            if drasi
                .get_graph()
                .await
                .unwrap()
                .nodes
                .iter()
                .any(|node| node.id == "reaction" && node.status == status)
            {
                return;
            }
            let event = events.recv().await.unwrap();
            if event.component_id == "reaction" && event.status == status {
                return;
            }
        }
    })
    .await
    .unwrap();
}

async fn blocked_worker(instance: &str, fail_checkpoint: bool, fail_effect: bool) {
    let metrics = RecordingMetrics::default();
    let (source, input) = ApplicationSource::new(
        "source",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )
    .unwrap();
    let entered = Arc::new(Notify::new());
    let permits = Arc::new(Semaphore::new(0));
    let store = Arc::new(CheckpointStore {
        inner: MemoryStateStoreProvider::new(),
        fail: AtomicBool::new(false),
    });
    let drasi = DrasiLib::builder()
        .with_id(instance)
        .with_telemetry(
            TelemetryOptions::new(
                TelemetrySettings::new(TelemetryProfile::Persistence)
                    .with_tracing(false)
                    .with_sampling(RootSampling::Never),
            )
            .with_metric_context(
                MetricRegistrationContext::new(Arc::new(metrics.clone()), Default::default())
                    .unwrap(),
            ),
        )
        .with_state_store_provider(store.clone())
        .with_source(source)
        .with_query(
            Query::cypher("query")
                .query("MATCH (n:Item) RETURN n.name AS name")
                .from_input("source")
                .enable_bootstrap(false)
                .auto_start(true)
                .build(),
        )
        .with_reaction(ControlledReaction {
            base: ReactionBase::new(ReactionBaseParams::new("reaction", vec!["query".into()])),
            entered: entered.clone(),
            permits: permits.clone(),
            fail_effect,
        })
        .build()
        .await
        .unwrap();
    drasi.start().await.unwrap();
    ready(&drasi, ComponentStatus::Running).await;
    let inputs = if fail_checkpoint || fail_effect { 1 } else { 2 };
    for id in ["one", "two"].into_iter().take(inputs) {
        input
            .send_node_insert(
                id,
                vec!["Item"],
                PropertyMapBuilder::new().with_string("name", id).build(),
            )
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    metrics
        .wait_for(|| {
            metrics.count("drasi.query.events_processed", &[("query_id", "query")]) == inputs as u64
        })
        .await;
    assert_eq!(metrics.count("drasi.reaction.results_processed", &[]), 0);
    metrics
        .wait_for(|| {
            metrics
                .gauge_samples("drasi.reaction.checkpoint_lag_events", &[])
                .last()
                == Some(&(inputs as f64))
        })
        .await;
    store.fail.store(fail_checkpoint, Ordering::Release);
    permits.add_permits(inputs);
    if fail_checkpoint || fail_effect {
        ready(&drasi, ComponentStatus::Error).await;
        drasi.shutdown().await.unwrap();
        assert_eq!(
            metrics.count("drasi.reaction.results_processed", &[]),
            u64::from(!fail_effect)
        );
        assert_eq!(
            metrics
                .gauge_samples("drasi.reaction.checkpoint_lag_events", &[])
                .last(),
            Some(&1.0)
        );
        assert_eq!(
            metrics.count("drasi.errors", &[("component_id", "reaction")]),
            1
        );
        let checkpoint = store
            .get("reaction", "checkpoint:query")
            .await
            .unwrap()
            .unwrap();
        let checkpoint: drasi_lib::reactions::ReactionCheckpoint =
            bincode::deserialize(&checkpoint).unwrap();
        assert_eq!(checkpoint.sequence, 0);
        return;
    }
    metrics
        .wait_for(|| {
            metrics.count("drasi.reaction.results_processed", &[]) == 2
                && metrics
                    .gauge_samples("drasi.reaction.checkpoint_lag_events", &[])
                    .last()
                    == Some(&0.0)
        })
        .await;
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("component_id", "reaction"), ("completion", "handled")]
        ),
        2
    );
    assert_eq!(
        metrics
            .histogram(
                "drasi.component.operation.duration",
                &[
                    ("component_id", "reaction"),
                    ("operation", "reaction"),
                    ("outcome", "success")
                ]
            )
            .len(),
        2
    );
    assert!(metrics
        .matching("drasi.pipeline.end_to_end.duration", &[])
        .is_empty());
    drasi.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn queued_handling_and_confirmed_lag_current_thread() {
    blocked_worker("worker-metrics-current", false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_handling_and_confirmed_lag_multi_thread() {
    blocked_worker("worker-metrics-multi", false, false).await;
}

struct CheckpointStore {
    inner: MemoryStateStoreProvider,
    fail: AtomicBool,
}

#[async_trait]
impl StateStoreProvider for CheckpointStore {
    async fn get(&self, store: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(store, key).await
    }
    async fn set(&self, store: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        if key.starts_with("checkpoint:") && self.fail.load(Ordering::Acquire) {
            return Err(StateStoreError::StorageError(
                "controlled checkpoint failure".into(),
            ));
        }
        self.inner.set(store, key, value).await
    }
    async fn delete(&self, store: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.delete(store, key).await
    }
    async fn contains_key(&self, store: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(store, key).await
    }
    async fn get_many(
        &self,
        store: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store, keys).await
    }
    async fn set_many(&self, store: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        self.inner.set_many(store, entries).await
    }
    async fn delete_many(&self, store: &str, keys: &[&str]) -> StateStoreResult<usize> {
        self.inner.delete_many(store, keys).await
    }
    async fn clear_store(&self, store: &str) -> StateStoreResult<usize> {
        self.inner.clear_store(store).await
    }
    async fn list_keys(&self, store: &str) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(store).await
    }
    async fn store_exists(&self, store: &str) -> StateStoreResult<bool> {
        self.inner.store_exists(store).await
    }
    async fn key_count(&self, store: &str) -> StateStoreResult<usize> {
        self.inner.key_count(store).await
    }
}

#[tokio::test]
async fn successful_handling_survives_a_failed_checkpoint_without_zero_lag() {
    blocked_worker("worker-metrics-checkpoint-error", true, false).await;
}

#[tokio::test]
async fn failed_handling_is_not_counted_or_reported_twice() {
    blocked_worker("worker-metrics-effect-error", false, true).await;
}
