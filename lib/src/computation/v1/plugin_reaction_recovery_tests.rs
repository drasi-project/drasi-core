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

//! The former reaction-manager recovery scenarios, exercised through the
//! graph's plugin host and actual native query outputs instead of a second engine.

use super::*;
use crate::{
    channels::ComponentStatusHandle,
    metrics::{LifecycleMetrics, QueryOutputMetrics, ReactionMetrics},
    state_store::{MemoryStateStoreProvider, StateStoreProvider},
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use std::{collections::HashMap, num::NonZeroUsize, sync::atomic::AtomicUsize};
use tokio::sync::Notify;

struct DurableCapabilityStore;

#[async_trait]
impl StateStoreProvider for DurableCapabilityStore {
    fn is_durable(&self) -> bool {
        true
    }
    async fn get(&self, _: &str, _: &str) -> crate::StateStoreResult<Option<Vec<u8>>> {
        Ok(None)
    }
    async fn set(&self, _: &str, _: &str, _: Vec<u8>) -> crate::StateStoreResult<()> {
        Ok(())
    }
    async fn delete(&self, _: &str, _: &str) -> crate::StateStoreResult<bool> {
        Ok(false)
    }
    async fn contains_key(&self, _: &str, _: &str) -> crate::StateStoreResult<bool> {
        Ok(false)
    }
    async fn get_many(
        &self,
        _: &str,
        _: &[&str],
    ) -> crate::StateStoreResult<HashMap<String, Vec<u8>>> {
        Ok(HashMap::new())
    }
    async fn set_many(&self, _: &str, _: &[(&str, &[u8])]) -> crate::StateStoreResult<()> {
        Ok(())
    }
    async fn delete_many(&self, _: &str, _: &[&str]) -> crate::StateStoreResult<usize> {
        Ok(0)
    }
    async fn clear_store(&self, _: &str) -> crate::StateStoreResult<usize> {
        Ok(0)
    }
    async fn list_keys(&self, _: &str) -> crate::StateStoreResult<Vec<String>> {
        Ok(Vec::new())
    }
    async fn store_exists(&self, _: &str) -> crate::StateStoreResult<bool> {
        Ok(false)
    }
    async fn key_count(&self, _: &str) -> crate::StateStoreResult<usize> {
        Ok(0)
    }
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}

struct TestReaction {
    queries: Vec<String>,
    durable: bool,
    snapshot: bool,
    policy: ReactionRecoveryPolicy,
    status: ComponentStatusHandle,
    delivered: Mutex<Vec<QueryResult>>,
    bootstraps: Mutex<Vec<(String, bool, u64)>>,
    fail_bootstrap: AtomicBool,
    start_gate: Option<Arc<Gate>>,
    bootstrap_gate: Mutex<Option<Arc<Gate>>>,
    bootstrap_active: AtomicUsize,
    bootstrap_max: AtomicUsize,
}

impl TestReaction {
    fn new(queries: &[&str], snapshot: bool, policy: ReactionRecoveryPolicy) -> Self {
        Self {
            queries: queries.iter().map(|id| id.to_string()).collect(),
            durable: false,
            snapshot,
            policy,
            status: ComponentStatusHandle::new("reaction"),
            delivered: Mutex::new(Vec::new()),
            bootstraps: Mutex::new(Vec::new()),
            fail_bootstrap: AtomicBool::new(false),
            start_gate: None,
            bootstrap_gate: Mutex::new(None),
            bootstrap_active: AtomicUsize::new(0),
            bootstrap_max: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Reaction for TestReaction {
    fn id(&self) -> &str {
        "reaction"
    }
    fn type_name(&self) -> &str {
        "recovery-test"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_ids(&self) -> Vec<String> {
        self.queries.clone()
    }
    fn is_durable(&self) -> bool {
        self.durable
    }
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        self.snapshot
    }
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.policy
    }
    fn auto_start(&self) -> bool {
        false
    }
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.status.wire(context.update_tx).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        if let Some(gate) = &self.start_gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.status.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.status.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.status.get_status().await
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.delivered.lock().unwrap().push(result);
        Ok(())
    }
    async fn bootstrap(&self, context: BootstrapContext) -> anyhow::Result<()> {
        let active = self.bootstrap_active.fetch_add(1, Ordering::AcqRel) + 1;
        self.bootstrap_max.fetch_max(active, Ordering::AcqRel);
        let gate = self.bootstrap_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        let snapshot = context.fetch_snapshot().await?;
        self.bootstraps.lock().unwrap().push((
            context.query_id.clone(),
            context.is_reset,
            snapshot.as_of_sequence,
        ));
        context
            .write_checkpoint(&ReactionCheckpoint {
                sequence: snapshot.as_of_sequence,
                config_hash: snapshot.config_hash,
            })
            .await?;
        self.bootstrap_active.fetch_sub(1, Ordering::AcqRel);
        anyhow::ensure!(
            !self.fail_bootstrap.load(Ordering::Acquire),
            "bootstrap hook failed"
        );
        Ok(())
    }
}

struct Harness {
    catalog: QueryResultsCatalog,
    queries: BTreeMap<String, ContinuousQueryTransformer>,
    store: Arc<MemoryStateStoreProvider>,
    _statuses: Vec<tokio::sync::watch::Sender<ComponentStatus>>,
}

impl Harness {
    async fn new(ids: &[&str], capacity: usize) -> Self {
        let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
        let mut queries = BTreeMap::new();
        let mut statuses = Vec::new();
        for id in ids {
            let (status, status_rx) = tokio::sync::watch::channel(ComponentStatus::Running);
            statuses.push(status);
            catalog
                .configure_delivery(
                    id,
                    crate::DispatchMode::Broadcast,
                    1,
                    123,
                    Arc::new(QueryOutputMetrics::new()),
                    status_rx,
                )
                .unwrap();
            let mut query = ContinuousQueryTransformer::new(
                ContinuousQueryDefinition {
                    graph_id: "reaction-recovery".into(),
                    id: ComponentId::try_new(*id).unwrap(),
                    query: "MATCH (n:Person) RETURN n.name AS name".into(),
                    language: ComputationQueryLanguage::Cypher,
                    output_stream: StreamId::try_new(format!("{id}/out")).unwrap(),
                    outbox_capacity: NonZeroUsize::new(capacity).unwrap(),
                },
                Arc::new(drasi_core::computation::InMemoryComputationProvider),
            )
            .await
            .unwrap()
            .with_result_catalog(&catalog)
            .unwrap();
            query.start().await.unwrap();
            queries.insert(id.to_string(), query);
        }
        Self {
            catalog,
            queries,
            store: Arc::new(MemoryStateStoreProvider::new()),
            _statuses: statuses,
        }
    }

    fn host(&self, reaction: Arc<TestReaction>) -> Arc<ReactionPluginHost> {
        self.host_with_store(reaction, Some(self.store.clone()))
    }

    fn host_with_store(
        &self,
        reaction: Arc<TestReaction>,
        store: Option<Arc<dyn StateStoreProvider>>,
    ) -> Arc<ReactionPluginHost> {
        ReactionPluginHost::for_runtime(
            reaction.clone(),
            LegacyPluginServices {
                state_store: store,
                ..LegacyPluginServices::empty("reaction-recovery")
            },
            self.catalog.clone(),
            ReactionPluginOptions {
                bootstrap_timeout_secs: 1,
                ..Default::default()
            },
            RuntimeReactionMetrics {
                queries: reaction
                    .queries
                    .iter()
                    .map(|id| (id.clone(), Arc::new(ReactionMetrics::new())))
                    .collect(),
                lifecycle: Arc::new(LifecycleMetrics::new()),
            },
        )
        .unwrap()
    }

    async fn emit(&mut self, id: &str, sequence: u64) -> ChangeEnvelope {
        let input = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: vec!["Person".into()].into(),
                        effective_from: sequence * 1000,
                    },
                    properties: ElementPropertyMap::from(
                        serde_json::json!({"name": sequence.to_string()}),
                    ),
                },
            },
            StreamId::try_new(format!("{id}/input")).unwrap(),
            sequence,
            None,
        )
        .unwrap();
        self.queries
            .get_mut(id)
            .unwrap()
            .transform(InputEnvelope {
                port: PortId::try_new("in").unwrap(),
                envelope: input,
            })
            .await
            .unwrap()
            .pop()
            .unwrap()
            .envelope
    }

    async fn checkpoint(&self, query: &str) -> Option<ReactionCheckpoint> {
        crate::reactions::checkpoint::read_checkpoint(self.store.as_ref(), "reaction", query)
            .await
            .unwrap()
    }

    async fn shutdown(mut self, host: &ReactionPluginHost) {
        host.shutdown().await.unwrap();
        for query in self.queries.values_mut() {
            query.stop().await.unwrap();
        }
    }
}

#[tokio::test]
async fn validation_rejects_durable_without_durable_store() {
    let harness = Harness::new(&["q1"], 4).await;
    for store in [None, Some(harness.store.clone() as Arc<dyn StateStoreProvider>)] {
        let mut reaction = TestReaction::new(&["q1"], false, ReactionRecoveryPolicy::Strict);
        reaction.durable = true;
        let host = harness.host_with_store(Arc::new(reaction), store);
        let error = host.validate_startup_configuration().await.unwrap_err();
        assert!(error.to_string().contains("durable state store"));
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn validation_rejects_snapshot_with_auto_skip_gap() {
    let harness = Harness::new(&["q1"], 4).await;
    let host = harness.host(Arc::new(TestReaction::new(
        &["q1"],
        true,
        ReactionRecoveryPolicy::AutoSkipGap,
    )));
    assert!(host
        .validate_startup_configuration()
        .await
        .unwrap_err()
        .to_string()
        .contains("AutoSkipGap"));
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn validation_rejects_durable_reaction_on_volatile_query() {
    let harness = Harness::new(&["q1"], 4).await;
    let mut reaction = TestReaction::new(&["q1"], false, ReactionRecoveryPolicy::Strict);
    reaction.durable = true;
    let host = harness.host_with_store(Arc::new(reaction), Some(Arc::new(DurableCapabilityStore)));
    assert!(host
        .validate_startup_configuration()
        .await
        .unwrap_err()
        .to_string()
        .contains("volatile output"));
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn validation_rejects_durable_store_before_volatile_query() {
    let harness = Harness::new(&["q1"], 4).await;
    let mut reaction = TestReaction::new(&["q1"], false, ReactionRecoveryPolicy::Strict);
    reaction.durable = true;
    let host = harness.host(Arc::new(reaction));
    let error = host
        .validate_startup_configuration()
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("state store is volatile"), "{error}");
    assert!(!error.contains("volatile output"));
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn validation_rejects_no_snapshot_with_auto_reset() {
    let harness = Harness::new(&["q1"], 4).await;
    let host = harness.host(Arc::new(TestReaction::new(
        &["q1"],
        false,
        ReactionRecoveryPolicy::AutoReset,
    )));
    assert!(host
        .validate_startup_configuration()
        .await
        .unwrap_err()
        .to_string()
        .contains("AutoReset"));
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn validation_allows_non_durable_no_snapshot_strict() {
    let harness = Harness::new(&["q1"], 4).await;
    let host = harness.host(Arc::new(TestReaction::new(
        &["q1"],
        false,
        ReactionRecoveryPolicy::Strict,
    )));
    host.validate_startup_configuration().await.unwrap();
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn fresh_start_no_snapshot_starts_at_seq_zero() {
    let harness = Harness::new(&["q1"], 4).await;
    let host = harness.host(Arc::new(TestReaction::new(
        &["q1"],
        false,
        ReactionRecoveryPolicy::Strict,
    )));
    host.start_component().await.unwrap();
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 0);
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn fresh_trigger_does_not_enqueue_retained_outbox() {
    let mut harness = Harness::new(&["q1"], 4).await;
    harness.emit("q1", 1).await;
    harness.emit("q1", 2).await;
    let reaction = Arc::new(TestReaction::new(
        &["q1"],
        false,
        ReactionRecoveryPolicy::Strict,
    ));
    let host = harness.host(reaction.clone());
    host.start_component().await.unwrap();
    assert!(reaction.delivered.lock().unwrap().is_empty());
    assert!(reaction.bootstraps.lock().unwrap().is_empty());
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 2);
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn subscribe_head_is_not_raised_by_later_outbox() {
    let mut harness = Harness::new(&["q1"], 4).await;
    harness.emit("q1", 1).await;
    let gate = Arc::new(Gate::default());
    let mut reaction = TestReaction::new(&["q1"], false, ReactionRecoveryPolicy::Strict);
    reaction.start_gate = Some(gate.clone());
    let reaction = Arc::new(reaction);
    let host = harness.host(reaction.clone());
    let start = tokio::spawn({
        let host = host.clone();
        async move { host.start_component().await }
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    let later = harness.emit("q1", 2).await;
    gate.release.notify_one();
    start.await.unwrap().unwrap();
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 1);
    host.handle_envelope(&later).await.unwrap();
    assert_eq!(reaction.delivered.lock().unwrap()[0].sequence, 2);
    assert_eq!(
        harness.checkpoint("q1").await.unwrap().sequence,
        1,
        "enqueue acceptance is not completed side effects"
    );
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn fresh_trigger_does_not_persist_zero_when_query_unavailable() {
    let harness = Harness::new(&[], 4).await;
    let host = harness.host(Arc::new(TestReaction::new(
        &["missing"],
        false,
        ReactionRecoveryPolicy::Strict,
    )));
    assert!(host.start_component().await.is_err());
    assert!(harness.checkpoint("missing").await.is_none());
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn fresh_trigger_broadcast_gap_during_gate_wait_recovers_after_subscription_head() {
    let mut harness = Harness::new(&["q1"], 8).await;
    let gate = Arc::new(Gate::default());
    let mut reaction = TestReaction::new(&["q1"], false, ReactionRecoveryPolicy::Strict);
    reaction.start_gate = Some(gate.clone());
    let reaction = Arc::new(reaction);
    let host = harness.host(reaction.clone());
    let mut subscription = harness.catalog.subscribe_query("q1").unwrap();
    let heads = BTreeMap::from([("q1".into(), subscription.head().unwrap())]);
    let start = tokio::spawn({
        let host = host.clone();
        async move { host.start_component_with_heads(heads).await }
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    for sequence in 1..=4 {
        let output = harness.emit("q1", sequence).await;
        harness.catalog.publish(output).await.unwrap();
    }
    gate.release.notify_one();
    start.await.unwrap().unwrap();
    assert!(
        subscription.receive().await.is_err(),
        "one-slot broadcast must report overflow"
    );
    host.recover_gap("q1").await.unwrap();
    assert_eq!(
        reaction
            .delivered
            .lock()
            .unwrap()
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 0);
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn outbox_catchup_replays_entries_to_reaction_without_acknowledging_side_effects() {
    let mut harness = Harness::new(&["q1"], 4).await;
    let reaction = Arc::new(TestReaction::new(
        &["q1"],
        false,
        ReactionRecoveryPolicy::Strict,
    ));
    let host = harness.host(reaction.clone());
    host.start_component().await.unwrap();
    host.stop_component().await.unwrap();
    harness.emit("q1", 1).await;
    harness.emit("q1", 2).await;
    host.start_component().await.unwrap();
    assert_eq!(
        reaction
            .delivered
            .lock()
            .unwrap()
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 0);
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn config_hash_mismatch_obeys_strict_and_auto_reset() {
    for policy in [ReactionRecoveryPolicy::Strict, ReactionRecoveryPolicy::AutoReset] {
        let mut harness = Harness::new(&["q1"], 4).await;
        harness.emit("q1", 1).await;
        let reaction = Arc::new(TestReaction::new(&["q1"], true, policy));
        let host = harness.host(reaction.clone());
        crate::reactions::checkpoint::write_checkpoint(
            harness.store.as_ref(),
            "reaction",
            "q1",
            &ReactionCheckpoint {
                sequence: 0,
                config_hash: 999,
            },
        )
        .await
        .unwrap();
        let result = host.start_component().await;
        if policy == ReactionRecoveryPolicy::Strict {
            assert!(result.unwrap_err().to_string().contains("Strict"));
            assert_eq!(harness.checkpoint("q1").await.unwrap().config_hash, 999);
        } else {
            result.unwrap();
            assert_eq!(
                reaction.bootstraps.lock().unwrap().as_slice(),
                &[("q1".into(), true, 1)]
            );
            assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 1);
        }
        harness.shutdown(&host).await;
    }
}

#[tokio::test]
async fn broadcast_gap_obeys_strict_auto_reset_and_auto_skip_gap() {
    for policy in [
        ReactionRecoveryPolicy::Strict,
        ReactionRecoveryPolicy::AutoReset,
        ReactionRecoveryPolicy::AutoSkipGap,
    ] {
        let mut harness = Harness::new(&["q1"], 1).await;
        let reaction = Arc::new(TestReaction::new(
            &["q1"],
            policy == ReactionRecoveryPolicy::AutoReset,
            policy,
        ));
        let host = harness.host(reaction.clone());
        host.start_component().await.unwrap();
        harness.emit("q1", 1).await;
        harness.emit("q1", 2).await;
        let result = host.recover_gap("q1").await;
        if policy == ReactionRecoveryPolicy::Strict {
            assert!(result.unwrap_err().to_string().contains("Strict"));
            assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 0);
        } else {
            result.unwrap();
            assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 2);
        }
        if policy == ReactionRecoveryPolicy::AutoReset {
            assert_eq!(
                reaction.bootstraps.lock().unwrap().last(),
                Some(&("q1".into(), true, 2))
            );
        }
        assert!(reaction.delivered.lock().unwrap().is_empty());
        harness.shutdown(&host).await;
    }
}

#[tokio::test]
async fn forwarder_filters_stale_events_after_gate_opens() {
    let mut harness = Harness::new(&["q1"], 4).await;
    let old = harness.emit("q1", 1).await;
    let reaction = Arc::new(TestReaction::new(
        &["q1"],
        false,
        ReactionRecoveryPolicy::Strict,
    ));
    let host = harness.host(reaction.clone());
    host.start_component().await.unwrap();
    host.handle_envelope(&old).await.unwrap();
    let new = harness.emit("q1", 2).await;
    host.handle_envelope(&new).await.unwrap();
    host.handle_envelope(&new).await.unwrap();
    assert_eq!(reaction.delivered.lock().unwrap().len(), 1);
    assert_eq!(reaction.delivered.lock().unwrap()[0].sequence, 2);
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn auto_reset_bootstrap_hook_failure_preserves_checkpoint_and_retries() {
    let mut harness = Harness::new(&["q1"], 1).await;
    let reaction = Arc::new(TestReaction::new(
        &["q1"],
        true,
        ReactionRecoveryPolicy::AutoReset,
    ));
    let host = harness.host(reaction.clone());
    host.start_component().await.unwrap();
    harness.emit("q1", 1).await;
    harness.emit("q1", 2).await;
    reaction.fail_bootstrap.store(true, Ordering::Release);
    assert!(host
        .recover_gap("q1")
        .await
        .unwrap_err()
        .to_string()
        .contains("bootstrap hook failed"));
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 0);
    reaction.fail_bootstrap.store(false, Ordering::Release);
    host.recover_gap("q1").await.unwrap();
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 2);
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn concurrent_gap_recovery_serialized_by_mutex() {
    let mut harness = Harness::new(&["q1", "q2"], 1).await;
    let reaction = Arc::new(TestReaction::new(
        &["q1", "q2"],
        true,
        ReactionRecoveryPolicy::AutoReset,
    ));
    let host = harness.host(reaction.clone());
    host.start_component().await.unwrap();
    for id in ["q1", "q2"] {
        harness.emit(id, 1).await;
        harness.emit(id, 2).await;
    }
    let gate = Arc::new(Gate::default());
    *reaction.bootstrap_gate.lock().unwrap() = Some(gate.clone());
    let first = tokio::spawn({
        let host = host.clone();
        async move { host.recover_gap("q1").await }
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    let second = tokio::spawn({
        let host = host.clone();
        async move { host.recover_gap("q2").await }
    });
    tokio::task::yield_now().await;
    assert_eq!(reaction.bootstrap_active.load(Ordering::Acquire), 1);
    gate.release.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(reaction.bootstrap_max.load(Ordering::Acquire), 1);
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 2);
    assert_eq!(harness.checkpoint("q2").await.unwrap().sequence, 2);
    harness.shutdown(&host).await;
}

#[tokio::test]
async fn auto_reset_bootstrap_snapshot_failure_propagates_without_advancing_checkpoint() {
    let mut harness = Harness::new(&["q1"], 1).await;
    let reaction = Arc::new(TestReaction::new(
        &["q1"],
        true,
        ReactionRecoveryPolicy::AutoReset,
    ));
    let host = harness.host(reaction);
    host.start_component().await.unwrap();
    harness.emit("q1", 1).await;
    harness.emit("q1", 2).await;
    let wrong = QueryChangeCodec::encode_row(
        "different-query",
        1,
        &BTreeMap::new(),
        QueryRowKind::Row,
        RecordImage::Full,
    )
    .unwrap();
    harness.queries["q1"]
        .results()
        .state
        .write()
        .unwrap()
        .rows
        .insert(wrong.identity().clone(), wrong);
    assert!(host.recover_gap("q1").await.is_err());
    assert_eq!(harness.checkpoint("q1").await.unwrap().sequence, 0);
    harness.shutdown(&host).await;
}
