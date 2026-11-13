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

#![cfg(test)]

#[path = "../../lib/tests/computation_reaction_support/mod.rs"]
mod computation_reaction_support;

use computation_reaction_support::{
    checkpoint, wait_checkpoint, wait_until, Gate, ProbeReaction, DEADLINE,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    computation::v1::*, CapacityPolicy, ComponentStatus, DrasiLib, DurabilityConfig,
    ReactionRecoveryPolicy, StateStoreProvider, StorageBackendRef,
};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

fn test_root() -> tempfile::TempDir {
    let parent =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/computation-reaction-recovery");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::Builder::new()
        .prefix("reaction-")
        .tempdir_in(parent)
        .unwrap()
}

fn reaction_id() -> drasi_core::ComponentId {
    drasi_core::ComponentId::try_new("00000000-0000-0000-0000-0000000004e1").expect("fixture UUID")
}

fn probe_reaction(queries: &[&str]) -> ProbeReaction {
    ProbeReaction::new(
        reaction_id(),
        queries
            .iter()
            .map(|name| {
                drasi_computation_contracts::ComponentReference::by_name(
                    drasi_core::ComponentName::try_new(*name).expect("fixture query name"),
                )
            })
            .collect(),
    )
}

struct Fixture {
    core: DrasiLib,
    source: ApplicationSourceHandle,
    state: Arc<RedbStateStoreProvider>,
}

impl Fixture {
    async fn new(root: &Path, queries: &[(&str, u128, &str, bool)]) -> Self {
        let persistent = queries.iter().any(|(_, _, _, persistent)| *persistent);
        let state = Arc::new(RedbStateStoreProvider::new(root.join("state.redb")).unwrap());
        let (source, handle) = ApplicationSource::new(
            drasi_core::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            ApplicationSourceConfig {
                properties: HashMap::new(),
                durability: persistent.then_some(DurabilityConfig {
                    enabled: true,
                    max_events: 4096,
                    capacity_policy: CapacityPolicy::RejectIncoming,
                }),
            },
        )
        .unwrap();
        let mut builder = lib_integration_tests::query_builder()
            .with_id("computation-reaction-recovery")
            .with_source(
                drasi_core::ComponentName::try_new("source").expect("fixture component name"),
                source,
            )
            .with_state_store_provider(state.clone());
        if persistent {
            let indexes = root.join("indexes");
            let wal = root.join("wal");
            std::fs::create_dir_all(&indexes).unwrap();
            std::fs::create_dir_all(&wal).unwrap();
            builder = builder
                .with_index_provider(
                    "rocks",
                    Arc::new(RocksDbIndexProvider::new(indexes, false, false)),
                )
                .with_wal_provider(Arc::new(RedbWalProvider::new(wal)));
        }
        for (name, id, text, persistent) in queries {
            let mut query = drasi_lib::Query::cypher(
                drasi_core::ComponentName::try_new(*name).expect("fixture component name"),
            )
            .with_id(ComponentId::try_new(format!("{id:032x}")).expect("fixture query UUID"))
            .query(*text)
            .from_input_by_name(
                drasi_core::ComponentName::try_new("source").expect("fixture component name"),
            )
            .enable_bootstrap(false)
            .auto_start(true)
            .with_outbox_capacity(64);
            if *persistent {
                query = query.with_storage_backend(StorageBackendRef::Named("rocks".into()));
            }
            builder = builder.with_query(query.build().expect("query configuration"));
        }
        let core = builder.build().await.unwrap();
        tokio::time::timeout(DEADLINE, core.start())
            .await
            .unwrap()
            .unwrap();
        for (name, _, _, _) in queries {
            let name = drasi_core::ComponentName::try_new(*name).expect("fixture component name");
            tokio::time::timeout(
                DEADLINE,
                core.component_by_name(&name).unwrap().wait_started(),
            )
            .await
            .unwrap()
            .unwrap();
        }
        Self {
            core,
            source: handle,
            state,
        }
    }

    async fn add(&self, reaction: ProbeReaction) -> ComponentHandle {
        let handle = self
            .core
            .add_reaction_with_handle(
                drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
                reaction,
            )
            .await
            .unwrap();
        tokio::time::timeout(DEADLINE, handle.wait_created())
            .await
            .unwrap()
            .unwrap();
        handle
    }

    fn component_id(&self, name: &str) -> drasi_core::ComponentId {
        self.core
            .component_by_name(
                &drasi_core::ComponentName::try_new(name).expect("fixture component name"),
            )
            .expect("fixture component")
            .id()
            .clone()
    }

    async fn checkpoint(&self, query: &str) -> Option<drasi_lib::ReactionCheckpoint> {
        checkpoint(
            self.state.as_ref(),
            &reaction_id(),
            &self.component_id(query),
        )
        .await
    }

    async fn wait_checkpoint(&self, query: &str, sequence: u64) {
        wait_checkpoint(
            self.state.as_ref(),
            &reaction_id(),
            &self.component_id(query),
            sequence,
        )
        .await;
    }

    async fn emit(&self, label: &str, value: &str) {
        self.source
            .send_node_insert(
                value,
                vec![label],
                PropertyMapBuilder::new()
                    .with_string("value", value)
                    .build(),
            )
            .await
            .unwrap();
    }

    async fn wait_sequence(&self, query: &drasi_core::ComponentName, sequence: u64) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if self
                    .core
                    .get_query_output_metrics_by_name(query)
                    .await
                    .unwrap()
                    .outbox_latest_seq
                    == sequence
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}

const QUERY: &str = "MATCH (n:Item) RETURN n.value AS value";

#[tokio::test]
async fn ordinary_fresh_trigger_keeps_arrivals_during_slow_start() {
    let root = test_root();
    let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, true)]).await;
    fixture.emit("Item", "old-1").await;
    fixture.emit("Item", "old-2").await;
    fixture
        .wait_sequence(
            &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            2,
        )
        .await;

    let mut reaction = probe_reaction(&["q"]);
    reaction.durable = true;
    let gate = Gate::new();
    reaction.start_gate = Some(gate.clone());
    let probe = reaction.probe.clone();
    fixture.add(reaction).await;
    let reaction_name =
        drasi_core::ComponentName::try_new("reaction").expect("fixture component name");
    let mut start = Box::pin(fixture.core.start_reaction_by_name(&reaction_name));
    tokio::time::timeout(DEADLINE, async {
        tokio::select! {
            _ = gate.entered.notified() => {},
            result = &mut start => panic!("reaction did not pause at startup: {result:?}"),
        }
    })
    .await
    .unwrap();
    fixture.emit("Item", "during-start").await;
    fixture
        .wait_sequence(
            &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            3,
        )
        .await;
    gate.release.add_permits(1);
    start.await.unwrap();
    wait_until(|| !probe.accepted.lock().unwrap().is_empty()).await;
    assert_eq!(
        probe
            .accepted
            .lock()
            .unwrap()
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(fixture.checkpoint("q").await.unwrap().sequence, 2);
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_queued_output_is_redelivered_after_instance_reconstruction() {
    let root = test_root();
    {
        let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, true)]).await;
        let mut reaction = probe_reaction(&["q"]);
        reaction.durable = true;
        let probe = reaction.probe.clone();
        fixture.add(reaction).await;
        fixture
            .core
            .start_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await
            .unwrap();
        fixture.emit("Item", "queued").await;
        wait_until(|| probe.accepted.lock().unwrap().len() == 1).await;
        assert!(probe.handled.lock().unwrap().is_empty());
        assert_eq!(fixture.checkpoint("q").await.unwrap().sequence, 0);
        fixture.core.shutdown().await.unwrap();
    }
    {
        let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, true)]).await;
        assert_eq!(
            fixture
                .core
                .get_query_results_by_name(
                    &drasi_core::ComponentName::try_new("q").expect("fixture component name")
                )
                .await
                .unwrap(),
            vec![serde_json::json!({ "value": "queued" })]
        );
        let mut reaction = probe_reaction(&["q"]);
        reaction.durable = true;
        reaction.handles_results = true;
        let probe = reaction.probe.clone();
        fixture.add(reaction).await;
        fixture
            .core
            .start_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await
            .unwrap();
        fixture.wait_checkpoint("q", 1).await;
        assert_eq!(probe.handled.lock().unwrap().len(), 1);
        assert_eq!(probe.handled.lock().unwrap()[0].sequence, 1);
        fixture
            .core
            .stop_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await
            .unwrap();
        fixture
            .core
            .start_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await
            .unwrap();
        assert_eq!(
            probe.accepted.lock().unwrap().len(),
            1,
            "handled output must not replay"
        );
        fixture.core.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_handler_failure_does_not_skip_the_failed_or_later_result() {
    let root = test_root();
    let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, true)]).await;
    let mut reaction = probe_reaction(&["q"]);
    reaction.durable = true;
    reaction.handles_results = true;
    reaction.probe.fail_effect.store(true, Ordering::SeqCst);
    let probe = reaction.probe.clone();
    let handle = fixture.add(reaction).await;
    fixture
        .core
        .start_reaction_by_name(
            &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
        )
        .await
        .unwrap();
    fixture.emit("Item", "first").await;
    fixture.emit("Item", "second").await;
    fixture
        .wait_sequence(
            &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            2,
        )
        .await;
    let mut observed = fixture
        .core
        .computation_control()
        .unwrap()
        .subscribe_observed();
    tokio::time::timeout(
        DEADLINE,
        observed.wait_for(|graph| graph.components[handle.id()].failure.is_some()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fixture.checkpoint("q").await.unwrap().sequence, 0);
    assert!(probe.handled.lock().unwrap().is_empty());
    fixture
        .core
        .start_reaction_by_name(
            &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
        )
        .await
        .unwrap();
    fixture.wait_checkpoint("q", 2).await;
    assert_eq!(
        probe
            .handled
            .lock()
            .unwrap()
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_durable_reaction_rejects_any_volatile_query_after_admission() {
    let root = test_root();
    let fixture = Fixture::new(
        root.path(),
        &[
            ("persistent", 0x141, QUERY, true),
            ("volatile", 0x142, QUERY, false),
        ],
    )
    .await;
    let mut reaction = probe_reaction(&["persistent", "volatile"]);
    reaction.durable = true;
    let probe = reaction.probe.clone();
    let handle = fixture.add(reaction).await;
    let error = fixture
        .core
        .start_reaction_by_name(
            &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("volatile"), "{error:#}");
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
    assert!(handle.observed().unwrap().failure.is_some());
    assert_eq!(
        fixture
            .core
            .get_reaction_status_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name")
            )
            .await
            .unwrap(),
        ComponentStatus::Error
    );
    assert_eq!(
        fixture
            .core
            .get_lifecycle_metrics()
            .await
            .unwrap()
            .startup_rejection_durable_on_volatile_query,
        1
    );
    assert!(fixture.checkpoint("persistent").await.is_none());
    assert!(fixture.checkpoint("volatile").await.is_none());
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_fresh_trigger_cannot_seed_checkpoint_zero_from_a_stopped_query() {
    let root = test_root();
    let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, true)]).await;
    fixture.emit("Item", "retained").await;
    fixture
        .wait_sequence(
            &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            1,
        )
        .await;
    fixture
        .core
        .stop_query_by_name(
            &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
        )
        .await
        .unwrap();
    let mut reaction = probe_reaction(&["q"]);
    reaction.durable = true;
    let probe = reaction.probe.clone();
    let handle = fixture.add(reaction).await;
    assert!(fixture
        .core
        .start_reaction_by_name(
            &drasi_core::ComponentName::try_new("reaction").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(handle.observed().unwrap().failure.is_some());
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
    assert!(fixture.checkpoint("q").await.is_none());
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_owned_adapter_checks_query_durability_not_only_its_state_store() {
    let root = test_root();
    let state = Arc::new(RedbStateStoreProvider::new(root.path().join("state.redb")).unwrap());
    assert!(state.is_durable());
    let catalog = QueryResultsCatalog::new("native-reaction").unwrap();
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "native-reaction".into(),
            id: ComponentId::try_new("4e0b9fcc-a53f-5962-9ef0-053c2cff6252").unwrap(),
            query: QUERY.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("q/out").unwrap(),
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .unwrap()
    .with_result_catalog(&catalog)
    .unwrap();
    query.start().await.unwrap();
    let query_id = query.descriptor().id().clone();
    let mut reaction = ProbeReaction::new(
        reaction_id(),
        vec![drasi_computation_contracts::ComponentReference::by_id(
            query_id.clone(),
        )],
    );
    reaction.durable = true;
    let probe = reaction.probe.clone();
    let mut services = LegacyPluginServices::empty("native-reaction");
    services.state_store = Some(state.clone());
    let host = ReactionPluginHost::owned(
        Box::new(reaction),
        services,
        catalog,
        ReactionPluginOptions::default(),
    )
    .unwrap();
    let mut adapter = ReactionPluginAdapter::new(
        ComponentId::try_new("0006e64c-92de-5d61-9d50-c79d45806b41").unwrap(),
        host.clone(),
    )
    .unwrap();
    let error = adapter.start().await.unwrap_err();
    assert!(format!("{error:#}").contains("volatile"), "{error:#}");
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
    assert!(checkpoint(state.as_ref(), &reaction_id(), &query_id)
        .await
        .is_none());
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn ordinary_bootstrap_completion_fences_its_snapshot_and_later_live_arrival() {
    let root = test_root();
    let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, true)]).await;
    fixture.emit("Item", "one").await;
    fixture.emit("Item", "two").await;
    fixture
        .wait_sequence(
            &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            2,
        )
        .await;
    let mut reaction = probe_reaction(&["q"]);
    reaction.durable = true;
    reaction.snapshot = true;
    reaction.policy = ReactionRecoveryPolicy::AutoReset;
    let gate = Gate::new();
    reaction.bootstrap_gate = Some(gate.clone());
    let probe = reaction.probe.clone();
    fixture.add(reaction).await;
    let reaction_name =
        drasi_core::ComponentName::try_new("reaction").expect("fixture component name");
    let mut start = Box::pin(fixture.core.start_reaction_by_name(&reaction_name));
    tokio::time::timeout(DEADLINE, async {
        tokio::select! {
            _ = gate.entered.notified() => {},
            result = &mut start => panic!("bootstrap did not pause: {result:?}"),
        }
    })
    .await
    .unwrap();
    assert!(fixture.checkpoint("q").await.is_none());
    fixture.emit("Item", "three").await;
    fixture
        .wait_sequence(
            &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            3,
        )
        .await;
    gate.release.add_permits(1);
    start.await.unwrap();
    wait_until(|| !probe.accepted.lock().unwrap().is_empty()).await;
    assert_eq!(fixture.checkpoint("q").await.unwrap().sequence, 2);
    assert_eq!(probe.accepted.lock().unwrap()[0].sequence, 3);
    {
        let snapshots = probe.snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].2, 2);
        assert_eq!(snapshots[0].3.len(), 2);
    }
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_multiple_query_subscriptions_keep_making_progress() {
    let root = test_root();
    let fixture = Fixture::new(
        root.path(),
        &[
            (
                "busy",
                0x13f,
                "MATCH (n:Busy) RETURN n.value AS value",
                false,
            ),
            (
                "sparse",
                0x140,
                "MATCH (n:Sparse) RETURN n.value AS value",
                false,
            ),
        ],
    )
    .await;
    let reaction = probe_reaction(&["busy", "sparse"]);
    let probe = reaction.probe.clone();
    fixture.add(reaction).await;
    fixture
        .core
        .start_reaction_by_name(
            &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
        )
        .await
        .unwrap();
    for index in 0..40 {
        fixture.emit("Busy", &format!("busy-{index}")).await;
        if index == 10 || index == 30 {
            fixture.emit("Sparse", &format!("sparse-{index}")).await;
        }
    }
    wait_until(|| probe.accepted.lock().unwrap().len() == 42).await;
    let accepted = probe.accepted.lock().unwrap().clone();
    assert_eq!(
        accepted
            .iter()
            .filter(|result| result.query_id
                == drasi_core::ComponentId::try_new("00000000-0000-0000-0000-00000000013f")
                    .expect("fixture UUID"))
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        (1..=40).collect::<Vec<_>>()
    );
    assert_eq!(
        accepted
            .iter()
            .filter(|result| result.query_id
                == drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000140")
                    .expect("fixture UUID"))
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(fixture.checkpoint("busy").await.unwrap().sequence, 0);
    assert_eq!(fixture.checkpoint("sparse").await.unwrap().sequence, 0);
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_auto_skip_recovers_a_reconstructed_volatile_query_but_strict_rejects_it() {
    for policy in [
        ReactionRecoveryPolicy::Strict,
        ReactionRecoveryPolicy::AutoSkipGap,
    ] {
        let root = test_root();
        let baseline = {
            let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, false)]).await;
            fixture.emit("Item", "old-lifetime").await;
            fixture
                .wait_sequence(
                    &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
                    1,
                )
                .await;
            let mut reaction = probe_reaction(&["q"]);
            reaction.policy = policy;
            fixture.add(reaction).await;
            fixture
                .core
                .start_reaction_by_name(
                    &drasi_core::ComponentName::try_new("reaction")
                        .expect("fixture component name"),
                )
                .await
                .unwrap();
            let baseline = fixture.checkpoint("q").await.unwrap();
            fixture.core.shutdown().await.unwrap();
            baseline
        };
        let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, false)]).await;
        fixture.emit("Item", "new-lifetime").await;
        fixture
            .wait_sequence(
                &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
                1,
            )
            .await;
        let mut reaction = probe_reaction(&["q"]);
        reaction.policy = policy;
        let probe = reaction.probe.clone();
        fixture.add(reaction).await;
        let result = fixture
            .core
            .start_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await;
        if policy == ReactionRecoveryPolicy::Strict {
            assert!(result.is_err());
        } else {
            result.unwrap();
            assert!(probe.accepted.lock().unwrap().is_empty());
            fixture.emit("Item", "new-live").await;
            wait_until(|| probe.accepted.lock().unwrap().len() == 1).await;
            fixture
                .core
                .stop_reaction_by_name(
                    &drasi_core::ComponentName::try_new("reaction")
                        .expect("fixture component name"),
                )
                .await
                .unwrap();
            fixture
                .core
                .start_reaction_by_name(
                    &drasi_core::ComponentName::try_new("reaction")
                        .expect("fixture component name"),
                )
                .await
                .unwrap();
            wait_until(|| probe.accepted.lock().unwrap().len() == 2).await;
            assert!(probe
                .accepted
                .lock()
                .unwrap()
                .iter()
                .all(|result| result.sequence == 2));
            assert!(probe.snapshots.lock().unwrap().is_empty());
        }
        assert_eq!(
            fixture.checkpoint("q").await,
            Some(baseline),
            "only the explicitly skipped head may be checkpointed, not later queued results"
        );
        fixture.core.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_auto_skip_recovers_a_query_configuration_reset_but_strict_rejects_it() {
    for policy in [
        ReactionRecoveryPolicy::Strict,
        ReactionRecoveryPolicy::AutoSkipGap,
    ] {
        let root = test_root();
        let fixture = Fixture::new(root.path(), &[("q", 0x13e, QUERY, true)]).await;
        fixture.emit("Item", "old-config").await;
        fixture
            .wait_sequence(
                &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
                1,
            )
            .await;
        let mut reaction = probe_reaction(&["q"]);
        reaction.durable = true;
        reaction.policy = policy;
        let probe = reaction.probe.clone();
        fixture.add(reaction).await;
        fixture
            .core
            .start_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await
            .unwrap();
        let baseline = fixture.checkpoint("q").await.unwrap();
        let previous = fixture
            .core
            .query_manager()
            .get_query_instance_by_name(
                &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            )
            .await
            .unwrap()
            .fetch_snapshot()
            .await
            .unwrap();
        let source_generation = fixture
            .core
            .component_by_name(
                &drasi_core::ComponentName::try_new("source").expect("fixture component name"),
            )
            .unwrap()
            .generation();
        fixture
            .core
            .stop_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await
            .unwrap();
        let updated = drasi_lib::Query::cypher(
            drasi_core::ComponentName::try_new("q").expect("fixture component name"),
        )
        .query("MATCH (n:Item) WHERE n.value <> 'old-config' RETURN n.value AS value")
        .from_input_by_name(
            drasi_core::ComponentName::try_new("source").expect("fixture component name"),
        )
        .enable_bootstrap(false)
        .auto_start(true)
        .with_outbox_capacity(64)
        .with_storage_backend(StorageBackendRef::Named("rocks".into()))
        .build()
        .expect("updated query configuration");
        fixture
            .core
            .update_query_by_name(
                &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
                updated,
            )
            .await
            .unwrap();
        tokio::time::timeout(
            DEADLINE,
            fixture
                .core
                .component_by_name(
                    &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
                )
                .unwrap()
                .wait_started(),
        )
        .await
        .unwrap()
        .unwrap();
        let reset = fixture
            .core
            .query_manager()
            .get_query_instance_by_name(
                &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
            )
            .await
            .unwrap()
            .fetch_snapshot()
            .await
            .unwrap();
        assert!(reset.output_generation > previous.output_generation);
        assert_ne!(reset.config_hash, previous.config_hash);
        assert!(
            reset.is_empty(),
            "same-column semantic changes must clear old rows"
        );
        assert_eq!(
            fixture
                .core
                .component_by_name(
                    &drasi_core::ComponentName::try_new("source").expect("fixture component name")
                )
                .unwrap()
                .observed()
                .unwrap()
                .generation,
            source_generation
        );
        fixture.emit("Item", "new-config-history").await;
        let skipped = reset.as_of_sequence + 1;
        fixture
            .wait_sequence(
                &drasi_core::ComponentName::try_new("q").expect("fixture component name"),
                skipped,
            )
            .await;

        let result = fixture
            .core
            .start_reaction_by_name(
                &drasi_core::ComponentName::try_new("reaction").expect("fixture component name"),
            )
            .await;
        if policy == ReactionRecoveryPolicy::Strict {
            assert!(result.is_err());
            assert_eq!(fixture.checkpoint("q").await, Some(baseline));
        } else {
            result.unwrap();
            assert!(probe.accepted.lock().unwrap().is_empty());
            let seeded = fixture.checkpoint("q").await.unwrap();
            assert_eq!(seeded.sequence, skipped);
            assert_eq!(seeded.config_hash, reset.config_hash);
            fixture.emit("Item", "new-config-live").await;
            wait_until(|| probe.accepted.lock().unwrap().len() == 1).await;
            fixture
                .core
                .stop_reaction_by_name(
                    &drasi_core::ComponentName::try_new("reaction")
                        .expect("fixture component name"),
                )
                .await
                .unwrap();
            fixture
                .core
                .start_reaction_by_name(
                    &drasi_core::ComponentName::try_new("reaction")
                        .expect("fixture component name"),
                )
                .await
                .unwrap();
            wait_until(|| probe.accepted.lock().unwrap().len() == 2).await;
            assert!(probe
                .accepted
                .lock()
                .unwrap()
                .iter()
                .all(|result| result.sequence == skipped + 1));
            assert_eq!(fixture.checkpoint("q").await, Some(seeded));
            assert!(probe.snapshots.lock().unwrap().is_empty());
        }
        fixture.core.shutdown().await.unwrap();
    }
}
