// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

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
    ComponentId, ComponentName, ComponentStatus, DrasiLib, MemoryStateStoreProvider, Query,
    Reaction, ReactionRuntimeContext, StorePartition,
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

const SOURCE_ID: &str = "00000000-0000-0000-0000-0000000005a1";
const QUERY_ID: &str = "00000000-0000-0000-0000-0000000005a2";
const REACTION_ID: &str = "00000000-0000-0000-0000-0000000005a3";

fn component_id(id: &str) -> ComponentId {
    ComponentId::try_new(id).expect("fixture UUID")
}

fn component_name(name: &str) -> ComponentName {
    ComponentName::try_new(name).expect("fixture component name")
}

struct ControlledReaction {
    base: ReactionBase,
    entered: Arc<Notify>,
    permits: Arc<Semaphore>,
    fail_effect: bool,
}

#[async_trait]
impl Reaction for ControlledReaction {
    fn id(&self) -> &ComponentId {
        self.base.get_id()
    }
    fn type_name(&self) -> &str {
        "controlled-metrics"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_inputs(&self) -> Vec<drasi_lib::config::ComponentReference> {
        self.base.query_inputs().to_vec()
    }
    fn auto_start(&self) -> bool {
        true
    }
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }
    async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
        self.base.bind_runtime_context(context).await;
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
    let reaction = component_id(REACTION_ID);
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = drasi.subscribe_all_component_events();
        loop {
            if drasi.get_graph().await.unwrap().nodes.iter().any(|node| {
                node.id == drasi_lib::computation::v1::GraphEntityId::Component(reaction.clone())
                    && node.status == status
            }) {
                return;
            }
            let event = events.recv().await.unwrap();
            if event.component_id == reaction && event.status == status {
                return;
            }
        }
    })
    .await
    .unwrap();
}

async fn blocked_worker(instance: &str, fail_checkpoint: bool, fail_effect: bool) {
    blocked_worker_with_timing(instance, fail_checkpoint, fail_effect, false).await;
}

async fn blocked_worker_with_timing(
    instance: &str,
    fail_checkpoint: bool,
    fail_effect: bool,
    timing: bool,
) {
    let metrics = RecordingMetrics::default();
    let (source, input) = ApplicationSource::new(
        component_id(SOURCE_ID),
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
    let mut settings = TelemetrySettings::new(TelemetryProfile::Persistence)
        .with_tracing(false)
        .with_sampling(RootSampling::Never);
    if timing {
        settings = settings.with_handled_branch_latency(Default::default());
    }
    let drasi = lib_integration_tests::query_builder()
        .with_id(instance)
        .with_telemetry(TelemetryOptions::new(settings).with_metric_context(
            MetricRegistrationContext::new(Arc::new(metrics.clone()), Default::default()).unwrap(),
        ))
        .with_state_store_provider(store.clone())
        .with_source(component_name("source"), source)
        .with_query(
            Query::cypher(component_name("query"))
                .with_id(component_id(QUERY_ID))
                .query("MATCH (n:Item) RETURN n.name AS name")
                .from_input_by_name(component_name("source"))
                .enable_bootstrap(false)
                .auto_start(true)
                .build()
                .unwrap(),
        )
        .with_reaction(
            component_name("reaction"),
            ControlledReaction {
                base: ReactionBase::new(ReactionBaseParams::new(
                    component_id(REACTION_ID),
                    vec![drasi_lib::config::ComponentReference::by_name(
                        component_name("query"),
                    )],
                )),
                entered: entered.clone(),
                permits: permits.clone(),
                fail_effect,
            },
        )
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
            metrics.count("drasi.query.events_processed", &[("query_id", QUERY_ID)])
                == inputs as u64
        })
        .await;
    assert_eq!(metrics.count("drasi.reaction.results_processed", &[]), 0);
    assert!(metrics
        .histogram("drasi.pipeline.end_to_end.duration", &[])
        .is_empty());
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
                .histogram("drasi.pipeline.end_to_end.duration", &[])
                .len(),
            usize::from(timing && !fail_effect)
        );
        assert_eq!(
            metrics
                .gauge_samples("drasi.reaction.checkpoint_lag_events", &[])
                .last(),
            Some(&1.0)
        );
        assert_eq!(
            metrics.count("drasi.errors", &[("component_id", REACTION_ID)]),
            1
        );
        let checkpoint = store
            .get(
                &StorePartition::component(component_id(REACTION_ID)),
                &format!("checkpoint:{QUERY_ID}"),
            )
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
            &[("component_id", REACTION_ID), ("completion", "handled")]
        ),
        2
    );
    assert_eq!(
        metrics
            .histogram(
                "drasi.component.operation.duration",
                &[
                    ("component_id", REACTION_ID),
                    ("operation", "reaction"),
                    ("outcome", "success")
                ]
            )
            .len(),
        2
    );
    if timing {
        metrics
            .wait_for(|| {
                metrics
                    .histogram("drasi.pipeline.end_to_end.duration", &[])
                    .len()
                    == 2
            })
            .await;
    } else {
        assert!(metrics
            .matching("drasi.pipeline.end_to_end.duration", &[])
            .is_empty());
    }
    drasi.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn handled_latency_counts_worker_completion_not_queue_acceptance() {
    blocked_worker_with_timing("worker-latency", false, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handled_latency_survives_checkpoint_failure_after_real_effects() {
    blocked_worker_with_timing("worker-latency-checkpoint", true, false, true).await;
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
    async fn get(
        &self,
        partition: &StorePartition,
        key: &str,
    ) -> StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(partition, key).await
    }
    async fn set(
        &self,
        partition: &StorePartition,
        key: &str,
        value: Vec<u8>,
    ) -> StateStoreResult<()> {
        if key.starts_with("checkpoint:") && self.fail.load(Ordering::Acquire) {
            return Err(StateStoreError::StorageError(
                "controlled checkpoint failure".into(),
            ));
        }
        self.inner.set(partition, key, value).await
    }
    async fn delete(&self, partition: &StorePartition, key: &str) -> StateStoreResult<bool> {
        self.inner.delete(partition, key).await
    }
    async fn contains_key(&self, partition: &StorePartition, key: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(partition, key).await
    }
    async fn get_many(
        &self,
        partition: &StorePartition,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(partition, keys).await
    }
    async fn set_many(
        &self,
        partition: &StorePartition,
        entries: &[(&str, &[u8])],
    ) -> StateStoreResult<()> {
        self.inner.set_many(partition, entries).await
    }
    async fn delete_many(
        &self,
        partition: &StorePartition,
        keys: &[&str],
    ) -> StateStoreResult<usize> {
        self.inner.delete_many(partition, keys).await
    }
    async fn clear_store(&self, partition: &StorePartition) -> StateStoreResult<usize> {
        self.inner.clear_store(partition).await
    }
    async fn list_keys(&self, partition: &StorePartition) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(partition).await
    }
    async fn store_exists(&self, partition: &StorePartition) -> StateStoreResult<bool> {
        self.inner.store_exists(partition).await
    }
    async fn key_count(&self, partition: &StorePartition) -> StateStoreResult<usize> {
        self.inner.key_count(partition).await
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
