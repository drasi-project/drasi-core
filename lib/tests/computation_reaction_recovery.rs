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

mod computation_reaction_support;

use computation_reaction_support::{checkpoint, wait_checkpoint, Gate, ProbeReaction, DEADLINE};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    computation::v1::*, ComponentStatus, MemoryStateStoreProvider, ReactionRecoveryPolicy,
    StateStoreProvider,
};
use std::{num::NonZeroUsize, sync::atomic::Ordering, sync::Arc};

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}

fn port(value: &str) -> PortId {
    PortId::try_new(value).unwrap()
}

async fn query(catalog: &QueryResultsCatalog, capacity: usize) -> ContinuousQueryTransformer {
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "reaction-recovery".into(),
            id: id("q"),
            query: "MATCH (n:Item) RETURN n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("q/out").unwrap(),
            outbox_capacity: NonZeroUsize::new(capacity).unwrap(),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .unwrap()
    .with_result_catalog(catalog)
    .unwrap();
    query.start().await.unwrap();
    query
}

async fn emit(query: &mut ContinuousQueryTransformer, sequence: u64, value: u64) -> ChangeEnvelope {
    let input = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &format!("item-{value}")),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence * 1000,
                },
                properties: ElementPropertyMap::from(serde_json::json!({ "value": value })),
            },
        },
        StreamId::try_new("source/out").unwrap(),
        sequence,
        None,
    )
    .unwrap();
    let mut results = query
        .transform(InputEnvelope {
            port: port("in"),
            envelope: input,
        })
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    results.remove(0).envelope
}

fn host(
    reaction: ProbeReaction,
    catalog: &QueryResultsCatalog,
    state: Arc<dyn StateStoreProvider>,
) -> (Arc<ReactionPluginHost>, ReactionPluginAdapter) {
    let mut services = LegacyPluginServices::empty("reaction-recovery");
    services.state_store = Some(state);
    let host = ReactionPluginHost::owned(
        Box::new(reaction),
        services,
        catalog.clone(),
        ReactionPluginOptions {
            bootstrap_timeout_secs: 5,
            ..Default::default()
        },
    )
    .unwrap();
    let adapter = ReactionPluginAdapter::new(id("sink"), host.clone()).unwrap();
    (host, adapter)
}

#[tokio::test]
async fn fresh_trigger_uses_the_pre_start_head_without_replaying_retained_history() {
    for policy in [ReactionRecoveryPolicy::Strict, ReactionRecoveryPolicy::AutoSkipGap] {
        fresh_trigger_during_start(policy, false).await;
    }
}

#[tokio::test]
async fn auto_skip_fresh_trigger_with_obsolete_metadata_keeps_arrivals_during_start() {
    fresh_trigger_during_start(ReactionRecoveryPolicy::AutoSkipGap, true).await;
}

async fn fresh_trigger_during_start(policy: ReactionRecoveryPolicy, obsolete_metadata: bool) {
    for capacity in [1, 8] {
        let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
        let mut query = query(&catalog, capacity).await;
        let _old = emit(&mut query, 1, 1).await;
        let retained = emit(&mut query, 2, 2).await;
        let state = Arc::new(MemoryStateStoreProvider::new());
        if obsolete_metadata {
            state
                .set(
                    "reaction",
                    "computation-query:71",
                    serde_json::to_vec(&serde_json::json!({
                        "config_hash": 0,
                        "reset_generation": 0,
                        "bootstrap_pending": false,
                        "incarnation": null
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        let mut reaction = ProbeReaction::new("reaction", &["q"]);
        reaction.policy = policy;
        let gate = Gate::new();
        reaction.start_gate = Some(gate.clone());
        let probe = reaction.probe.clone();
        let (host, mut adapter) = host(reaction, &catalog, state.clone());
        let mut start = Box::pin(adapter.start());
        tokio::time::timeout(DEADLINE, async {
            tokio::select! {
                _ = gate.entered.notified() => {},
                result = &mut start => panic!("reaction did not wait at startup: {result:?}"),
            }
        })
        .await
        .unwrap();
        let during_start = emit(&mut query, 3, 3).await;
        gate.release.add_permits(1);
        start.await.unwrap();

        assert!(probe.accepted.lock().unwrap().is_empty());
        assert_eq!(
            checkpoint(state.as_ref(), "reaction", "q")
                .await
                .unwrap()
                .sequence,
            2
        );
        adapter
            .handle(InputEnvelope {
                port: port("in"),
                envelope: retained,
            })
            .await
            .unwrap();
        adapter
            .handle(InputEnvelope {
                port: port("in"),
                envelope: during_start,
            })
            .await
            .unwrap();
        assert_eq!(
            probe
                .accepted
                .lock()
                .unwrap()
                .iter()
                .map(|result| result.sequence)
                .collect::<Vec<_>>(),
            vec![3],
            "only the arrival after the captured head is live, even during a slow start"
        );
        assert_eq!(
            checkpoint(state.as_ref(), "reaction", "q")
                .await
                .unwrap()
                .sequence,
            2
        );
        adapter.stop().await.unwrap();
        host.shutdown().await.unwrap();
        query.stop().await.unwrap();
    }
}

#[tokio::test]
async fn accepted_but_unhandled_results_replay_on_every_restart() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut query = query(&catalog, 8).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let reaction = ProbeReaction::new("reaction", &["q"]);
    let probe = reaction.probe.clone();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    adapter.start().await.unwrap();
    let baseline = checkpoint(state.as_ref(), "reaction", "q").await.unwrap();
    assert_eq!(baseline.sequence, 0);
    adapter
        .handle(InputEnvelope {
            port: port("in"),
            envelope: emit(&mut query, 1, 1).await,
        })
        .await
        .unwrap();
    for accepted_count in 1..=3 {
        assert_eq!(probe.accepted.lock().unwrap().len(), accepted_count);
        assert!(probe.handled.lock().unwrap().is_empty());
        let saved = checkpoint(state.as_ref(), "reaction", "q").await.unwrap();
        assert_eq!(saved.sequence, baseline.sequence);
        assert_eq!(saved.config_hash, baseline.config_hash);
        adapter.stop().await.unwrap();
        if accepted_count < 3 {
            adapter.start().await.unwrap();
        }
    }
    assert!(probe
        .accepted
        .lock()
        .unwrap()
        .iter()
        .all(|result| result.sequence == 1));
    host.shutdown().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn strict_recovers_transport_loss_from_accepted_tail_without_checkpointing_enqueues() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut query = query(&catalog, 2).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let reaction = ProbeReaction::new("reaction", &["q"]);
    let probe = reaction.probe.clone();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    adapter.start().await.unwrap();
    adapter
        .handle(InputEnvelope {
            port: port("in"),
            envelope: emit(&mut query, 1, 1).await,
        })
        .await
        .unwrap();
    emit(&mut query, 2, 2).await;
    let after_lag = emit(&mut query, 3, 3).await;
    assert!(
        query.results().replay(0).is_err(),
        "the handled checkpoint can lag retention while its input is still in the reaction queue"
    );
    adapter
        .handle(InputEnvelope {
            port: port("in"),
            envelope: after_lag,
        })
        .await
        .unwrap();
    assert_eq!(
        probe
            .accepted
            .lock()
            .unwrap()
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(probe.handled.lock().unwrap().is_empty());
    assert_eq!(
        checkpoint(state.as_ref(), "reaction", "q")
            .await
            .unwrap()
            .sequence,
        0
    );
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn strict_rejects_unretained_transport_loss_without_later_deliveries() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut query = query(&catalog, 1).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let reaction = ProbeReaction::new("reaction", &["q"]);
    let probe = reaction.probe.clone();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    adapter.start().await.unwrap();
    adapter
        .handle(InputEnvelope {
            port: port("in"),
            envelope: emit(&mut query, 1, 1).await,
        })
        .await
        .unwrap();
    emit(&mut query, 2, 2).await;
    for sequence in 3..=4 {
        let error = adapter
            .handle(InputEnvelope {
                port: port("in"),
                envelope: emit(&mut query, sequence, sequence).await,
            })
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("Strict"), "{error:#}");
    }
    assert_eq!(
        probe
            .accepted
            .lock()
            .unwrap()
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        checkpoint(state.as_ref(), "reaction", "q")
            .await
            .unwrap()
            .sequence,
        0
    );
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn an_explicit_gap_skip_does_not_acknowledge_later_queued_results() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut query = query(&catalog, 1).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let mut reaction = ProbeReaction::new("reaction", &["q"]);
    reaction.policy = ReactionRecoveryPolicy::AutoSkipGap;
    let probe = reaction.probe.clone();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    adapter.start().await.unwrap();
    adapter.stop().await.unwrap();
    emit(&mut query, 1, 1).await;
    emit(&mut query, 2, 2).await;
    adapter.start().await.unwrap();
    assert!(probe.accepted.lock().unwrap().is_empty());
    assert_eq!(
        checkpoint(state.as_ref(), "reaction", "q")
            .await
            .unwrap()
            .sequence,
        2
    );

    adapter
        .handle(InputEnvelope {
            port: port("in"),
            envelope: emit(&mut query, 3, 3).await,
        })
        .await
        .unwrap();
    adapter.stop().await.unwrap();
    adapter.start().await.unwrap();
    assert_eq!(
        probe
            .accepted
            .lock()
            .unwrap()
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![3, 3]
    );
    assert_eq!(
        checkpoint(state.as_ref(), "reaction", "q")
            .await
            .unwrap()
            .sequence,
        2,
        "only the explicit unavailable-history skip is persisted"
    );
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn replacing_a_query_during_reaction_start_does_not_seed_an_old_head() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut first = query(&catalog, 8).await;
    emit(&mut first, 1, 1).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let mut reaction = ProbeReaction::new("reaction", &["q"]);
    let gate = Gate::new();
    reaction.start_gate = Some(gate.clone());
    let probe = reaction.probe.clone();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    let mut start = Box::pin(adapter.start());
    tokio::time::timeout(DEADLINE, async {
        tokio::select! {
            _ = gate.entered.notified() => {},
            result = &mut start => panic!("reaction did not pause: {result:?}"),
        }
    })
    .await
    .unwrap();
    first.stop().await.unwrap();
    drop(first);
    let mut replacement = query(&catalog, 8).await;
    emit(&mut replacement, 1, 99).await;
    gate.release.add_permits(1);
    let error = start.await.unwrap_err();
    assert!(format!("{error:#}").contains("changed"), "{error:#}");
    assert!(checkpoint(state.as_ref(), "reaction", "q").await.is_none());
    assert!(probe.accepted.lock().unwrap().is_empty());
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    replacement.stop().await.unwrap();
}

#[tokio::test]
async fn handler_failure_does_not_advance_progress_and_is_redelivered() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut query = query(&catalog, 8).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let mut reaction = ProbeReaction::new("reaction", &["q"]);
    reaction.handles_results = true;
    reaction.probe.fail_effect.store(true, Ordering::SeqCst);
    let probe = reaction.probe.clone();
    let mut status = reaction.base.status_handle().subscribe_status();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    adapter.start().await.unwrap();
    if let Err(error) = adapter
        .handle(InputEnvelope {
            port: port("in"),
            envelope: emit(&mut query, 1, 1).await,
        })
        .await
    {
        assert!(format!("{error:#}").contains("effect"), "{error:#}");
    }
    tokio::time::timeout(
        DEADLINE,
        status.wait_for(|status| *status == ComponentStatus::Error),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        checkpoint(state.as_ref(), "reaction", "q")
            .await
            .unwrap()
            .sequence,
        0
    );
    assert!(probe.handled.lock().unwrap().is_empty());
    if let Err(error) = adapter.stop().await {
        assert!(format!("{error:#}").contains("effect"), "{error:#}");
        adapter.stop().await.unwrap();
    }
    adapter.start().await.unwrap();
    wait_checkpoint(state.as_ref(), "reaction", "q", 1).await;
    assert_eq!(
        *probe.attempts.lock().unwrap(),
        vec![("q".into(), 1), ("q".into(), 1)]
    );
    assert_eq!(probe.handled.lock().unwrap().len(), 1);
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn bootstrap_checkpoint_waits_for_success_and_retries_a_partial_snapshot() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut query = query(&catalog, 8).await;
    emit(&mut query, 1, 1).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let mut reaction = ProbeReaction::new("reaction", &["q"]);
    reaction.snapshot = true;
    reaction.policy = ReactionRecoveryPolicy::AutoReset;
    reaction.probe.fail_bootstrap.store(true, Ordering::SeqCst);
    let gate = Gate::new();
    reaction.bootstrap_gate = Some(gate.clone());
    let probe = reaction.probe.clone();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    let mut start = Box::pin(adapter.start());
    tokio::time::timeout(DEADLINE, async {
        tokio::select! {
            _ = gate.entered.notified() => {},
            result = &mut start => panic!("bootstrap did not block: {result:?}"),
        }
    })
    .await
    .unwrap();
    assert!(checkpoint(state.as_ref(), "reaction", "q").await.is_none());
    gate.release.add_permits(1);
    let error = start.await.unwrap_err();
    assert!(format!("{error:#}").contains("bootstrap failure"));
    assert!(checkpoint(state.as_ref(), "reaction", "q").await.is_none());
    adapter.stop().await.unwrap();
    adapter.start().await.unwrap();
    assert_eq!(
        checkpoint(state.as_ref(), "reaction", "q")
            .await
            .unwrap()
            .sequence,
        1
    );
    {
        let snapshots = probe.snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 2);
        assert!(!snapshots[0].1);
        assert!(snapshots[1].1);
        assert_eq!(snapshots[1].3, vec![serde_json::json!({ "value": 1 })]);
    }
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn a_new_volatile_query_incarnation_requires_snapshot_recovery() {
    let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
    let mut first = query(&catalog, 8).await;
    emit(&mut first, 1, 1).await;
    let state = Arc::new(MemoryStateStoreProvider::new());
    let mut reaction = ProbeReaction::new("reaction", &["q"]);
    reaction.snapshot = true;
    reaction.policy = ReactionRecoveryPolicy::AutoReset;
    let probe = reaction.probe.clone();
    let (host, mut adapter) = host(reaction, &catalog, state.clone());
    adapter.start().await.unwrap();
    adapter.stop().await.unwrap();
    first.stop().await.unwrap();
    drop(first);

    let mut replacement = query(&catalog, 8).await;
    emit(&mut replacement, 1, 99).await;
    adapter.start().await.unwrap();
    {
        let snapshots = probe.snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 2);
        assert!(!snapshots[0].1);
        assert!(snapshots[1].1);
        assert_eq!(snapshots[1].3, vec![serde_json::json!({ "value": 99 })]);
    }
    assert_eq!(
        checkpoint(state.as_ref(), "reaction", "q")
            .await
            .unwrap()
            .sequence,
        1
    );
    adapter.stop().await.unwrap();
    host.shutdown().await.unwrap();
    replacement.stop().await.unwrap();
}

#[tokio::test]
async fn auto_skip_rebases_a_checkpoint_ahead_of_output_but_strict_preserves_it() {
    for policy in [ReactionRecoveryPolicy::Strict, ReactionRecoveryPolicy::AutoSkipGap] {
        let catalog = QueryResultsCatalog::new("reaction-recovery").unwrap();
        let mut query = query(&catalog, 8).await;
        emit(&mut query, 1, 1).await;
        let state = Arc::new(MemoryStateStoreProvider::new());
        let mut reaction = ProbeReaction::new("reaction", &["q"]);
        reaction.policy = policy;
        let probe = reaction.probe.clone();
        let (host, mut adapter) = host(reaction, &catalog, state.clone());
        adapter.start().await.unwrap();
        adapter.stop().await.unwrap();
        let baseline = checkpoint(state.as_ref(), "reaction", "q").await.unwrap();
        let ahead = drasi_lib::ReactionCheckpoint {
            sequence: 100,
            config_hash: baseline.config_hash,
        };
        state
            .set(
                "reaction",
                "checkpoint:q",
                bincode::serialize(&ahead).unwrap(),
            )
            .await
            .unwrap();

        let result = adapter.start().await;
        if policy == ReactionRecoveryPolicy::Strict {
            assert!(result.is_err());
            assert_eq!(
                checkpoint(state.as_ref(), "reaction", "q").await,
                Some(ahead)
            );
        } else {
            result.unwrap();
            assert_eq!(
                checkpoint(state.as_ref(), "reaction", "q").await,
                Some(baseline.clone())
            );
            assert!(probe.accepted.lock().unwrap().is_empty());
            adapter
                .handle(InputEnvelope {
                    port: port("in"),
                    envelope: emit(&mut query, 2, 2).await,
                })
                .await
                .unwrap();
            adapter.stop().await.unwrap();
            adapter.start().await.unwrap();
            assert_eq!(
                probe
                    .accepted
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|result| result.sequence)
                    .collect::<Vec<_>>(),
                vec![2, 2],
                "rebasing an explicit skip must not checkpoint later enqueue acceptance"
            );
            assert_eq!(
                checkpoint(state.as_ref(), "reaction", "q").await,
                Some(baseline)
            );
        }
        adapter.stop().await.unwrap();
        host.shutdown().await.unwrap();
        query.stop().await.unwrap();
    }
}
