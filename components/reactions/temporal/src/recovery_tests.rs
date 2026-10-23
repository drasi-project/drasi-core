// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Result};
use async_trait::async_trait;
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    component_graph::ComponentGraph,
    context::ReactionRuntimeContext,
    ComponentStatus, MemoryStateStoreProvider, Reaction, ReactionCheckpoint, StateStoreError,
    StateStoreProvider, StateStoreResult,
};
use serde_json::{json, Value};

use crate::{
    client::Transport, config::Action, render::Command, TemporalReaction, TemporalReactionConfig,
};

const PLAN: &str = "temporal:pending:q";
const PROGRESS: &str = "temporal:progress:q";
const CHECKPOINT: &str = "checkpoint:q";

struct WriteFault {
    key: &'static str,
    remaining: usize,
    after_write: bool,
}

#[derive(Default)]
struct TestStore {
    inner: MemoryStateStoreProvider,
    fault: Mutex<Option<WriteFault>>,
    writes: Mutex<Vec<(String, usize)>>,
}

impl TestStore {
    fn fail_write(&self, key: &'static str, nth: usize, after_write: bool) {
        *self.fault.lock().unwrap() = Some(WriteFault {
            key,
            remaining: nth,
            after_write,
        });
    }

    async fn json(&self, key: &str) -> Value {
        serde_json::from_slice(&self.get("r", key).await.unwrap().unwrap()).unwrap()
    }
}

#[async_trait]
impl StateStoreProvider for TestStore {
    async fn get(&self, s: &str, k: &str) -> StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(s, k).await
    }
    async fn set(&self, s: &str, k: &str, v: Vec<u8>) -> StateStoreResult<()> {
        let failure = {
            let mut fault = self.fault.lock().unwrap();
            if let Some(f) = fault.as_mut().filter(|f| f.key == k) {
                f.remaining -= 1;
                if f.remaining == 0 {
                    fault.take().map(|f| f.after_write)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if failure != Some(false) {
            self.writes.lock().unwrap().push((k.to_owned(), v.len()));
            self.inner.set(s, k, v).await?;
        }
        if failure.is_some() {
            return Err(StateStoreError::StorageError(format!(
                "injected write failure: {k}"
            )));
        }
        Ok(())
    }
    async fn delete(&self, s: &str, k: &str) -> StateStoreResult<bool> {
        self.inner.delete(s, k).await
    }
    async fn contains_key(&self, s: &str, k: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(s, k).await
    }
    async fn get_many(&self, s: &str, k: &[&str]) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(s, k).await
    }
    async fn set_many(&self, s: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        for (k, v) in entries {
            self.set(s, k, v.to_vec()).await?;
        }
        Ok(())
    }
    async fn delete_many(&self, s: &str, k: &[&str]) -> StateStoreResult<usize> {
        self.inner.delete_many(s, k).await
    }
    async fn clear_store(&self, s: &str) -> StateStoreResult<usize> {
        self.inner.clear_store(s).await
    }
    async fn list_keys(&self, s: &str) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(s).await
    }
    async fn store_exists(&self, s: &str) -> StateStoreResult<bool> {
        self.inner.store_exists(s).await
    }
    async fn key_count(&self, s: &str) -> StateStoreResult<usize> {
        self.inner.key_count(s).await
    }
}

#[derive(Default)]
struct ServerState {
    attempts: Vec<Command>,
    effects: Vec<Command>,
    accepted: HashSet<String>,
    closed_runs: HashSet<(String, String)>,
    current_run: String,
    resolutions: usize,
    fail_attempt: Option<(usize, bool)>,
}

#[derive(Clone)]
struct Server(Arc<Mutex<ServerState>>, Arc<TestStore>);

impl Server {
    fn new(store: Arc<TestStore>) -> Self {
        Self(
            Arc::new(Mutex::new(ServerState {
                current_run: "original-run".into(),
                ..Default::default()
            })),
            store,
        )
    }
}

#[async_trait]
impl Transport for Server {
    async fn resolve_run(&self, _: &TemporalReactionConfig, _: &Command) -> Result<String> {
        let mut state = self.0.lock().unwrap();
        state.resolutions += 1;
        Ok(state.current_run.clone())
    }

    async fn send(&self, _: &TemporalReactionConfig, command: &Command) -> Result<()> {
        let plan = self.1.json(PLAN).await;
        assert!(
            plan["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|saved| saved["request_id"] == command.request_id),
            "the request identity must be durable before the RPC"
        );
        if matches!(
            command.action,
            Action::Cancel { .. } | Action::Terminate { .. }
        ) {
            let progress = self.1.json(PROGRESS).await;
            assert_eq!(progress["sequence"], plan["sequence"]);
            assert_eq!(
                progress["resolved_run_id"],
                json!(command.resolved_run_id),
                "the target run must be durable before the RPC"
            );
        }
        let mut state = self.0.lock().unwrap();
        state.attempts.push(command.clone());
        let failure = state
            .fail_attempt
            .filter(|(nth, _)| *nth == state.attempts.len());
        if failure == Some((state.attempts.len(), false)) {
            bail!("injected RPC failure before acceptance");
        }
        let applied = if matches!(
            command.action,
            Action::Cancel { .. } | Action::Terminate { .. }
        ) {
            let run = command
                .resolved_run_id
                .as_ref()
                .expect("destructive request must pin its run");
            state
                .closed_runs
                .insert((command.action.workflow_id().into(), run.clone()))
        } else {
            state.accepted.insert(command.request_id.clone())
        };
        if applied {
            state.effects.push(command.clone());
        }
        if failure.is_some() {
            bail!("injected RPC response loss after acceptance");
        }
        Ok(())
    }
}

fn config(action: &str) -> TemporalReactionConfig {
    let mut spec = json!({
        "action":action, "workflowId":"{{after.id}}",
    });
    match action {
        "signal" => spec["signalName"] = json!("notify"),
        "start" => {
            spec["workflowType"] = json!("TestWorkflow");
            spec["taskQueue"] = json!("test");
        }
        "update" => spec["updateName"] = json!("change"),
        "cancel" | "terminate" => spec["reason"] = json!("test"),
        _ => panic!("unknown test action"),
    }
    serde_json::from_value(json!({"outputTemplates":{"routes":{"q":{"added":spec}}}})).unwrap()
}

fn emission(sequence: u64, count: usize) -> QueryResult {
    QueryResult {
        query_id: "q".into(),
        sequence,
        timestamp: chrono::Utc::now(),
        results: (0..count)
            .map(|id| ResultDiff::Add {
                data: json!({"id":format!("target-{id}"), "value":"x".repeat(1024)}),
                row_signature: id as u64,
            })
            .collect(),
        metadata: Default::default(),
        profiling: None,
    }
}

async fn initialized(store: Arc<TestStore>, config: TemporalReactionConfig) -> TemporalReaction {
    let reaction = TemporalReaction::builder("r")
        .with_query("q")
        .with_config(config)
        .build()
        .unwrap();
    let (graph, _rx) = ComponentGraph::new("test");
    reaction
        .initialize(ReactionRuntimeContext::new(
            "test",
            "r",
            Some(store),
            graph.update_sender(),
            None,
        ))
        .await;
    if reaction.base.read_checkpoint("q").await.unwrap().is_none() {
        reaction
            .base
            .write_checkpoint(
                "q",
                &ReactionCheckpoint {
                    sequence: 0,
                    config_hash: 42,
                },
            )
            .await
            .unwrap();
    }
    reaction
}

async fn wait_for(reaction: &TemporalReaction, sequence: u64, expected: ComponentStatus) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if reaction.status().await == expected
                && reaction
                    .base
                    .read_checkpoint("q")
                    .await
                    .unwrap()
                    .unwrap()
                    .sequence
                    == sequence
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("reaction did not reach the expected status and checkpoint");
}

#[tokio::test]
async fn delivery_writes_one_plan_and_linear_progress_bytes() {
    for count in [32, 64] {
        let store = Arc::new(TestStore::default());
        let server = Server::new(store.clone());
        let reaction = initialized(store.clone(), config("signal")).await;
        store.writes.lock().unwrap().clear();
        reaction.start_with_client(server.clone()).await.unwrap();
        reaction
            .enqueue_query_result(emission(1, count))
            .await
            .unwrap();
        wait_for(&reaction, 1, ComponentStatus::Running).await;
        reaction.stop().await.unwrap();
        assert_eq!(server.0.lock().unwrap().effects.len(), count);
        let writes = store.writes.lock().unwrap();
        let plans: Vec<_> = writes.iter().filter(|(key, _)| key == PLAN).collect();
        assert_eq!(
            plans.len(),
            1,
            "the command vector must be persisted only once"
        );
        let total: usize = writes.iter().map(|(_, bytes)| bytes).sum();
        assert!(
            total <= plans[0].1 + count * 256 + 128,
            "progress rewrote payload bytes: {total}"
        );
    }
}

#[tokio::test]
async fn partial_rpc_failure_replays_frozen_commands_and_skips_persisted_successes() {
    for action in ["signal", "start", "update", "cancel", "terminate"] {
        for after_acceptance in [false, true] {
            let store = Arc::new(TestStore::default());
            let server = Server::new(store.clone());
            server.0.lock().unwrap().fail_attempt = Some((2, after_acceptance));
            let reaction = initialized(store.clone(), config(action)).await;
            reaction.start_with_client(server.clone()).await.unwrap();
            reaction.enqueue_query_result(emission(1, 3)).await.unwrap();
            wait_for(&reaction, 0, ComponentStatus::Error).await;
            reaction.stop().await.unwrap();
            let saved = store.json(PLAN).await;
            assert_eq!(store.json(PROGRESS).await["next"], 1);
            assert_eq!(server.0.lock().unwrap().attempts.len(), 2);
            server.0.lock().unwrap().fail_attempt = None;

            let replay = initialized(store.clone(), config(action)).await;
            replay.start_with_client(server.clone()).await.unwrap();
            let mut changed_input = emission(1, 3);
            changed_input.results.reverse();
            replay.enqueue_query_result(changed_input).await.unwrap();
            wait_for(&replay, 1, ComponentStatus::Running).await;
            replay.stop().await.unwrap();

            assert_eq!(
                store.json(PLAN).await,
                saved,
                "replay must not rewrite the immutable plan"
            );
            let state = server.0.lock().unwrap();
            assert_eq!(state.attempts.len(), 4);
            assert_eq!(state.effects.len(), 3);
            for (attempt, original) in state.attempts.iter().zip([0, 1, 1, 2]) {
                let saved_command = &saved["commands"][original];
                assert_eq!(attempt.request_id, saved_command["request_id"]);
                assert_eq!(json!(attempt.action), saved_command["action"]);
                assert_eq!(json!(attempt.payload), saved_command["payload"]);
            }
        }
    }
}

#[tokio::test]
async fn plan_write_failure_sends_nothing_and_preserves_committed_identities() {
    for after_write in [false, true] {
        let store = Arc::new(TestStore::default());
        let server = Server::new(store.clone());
        let reaction = initialized(store.clone(), config("signal")).await;
        store.fail_write(PLAN, 1, after_write);
        reaction.start_with_client(server.clone()).await.unwrap();
        reaction.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&reaction, 0, ComponentStatus::Error).await;
        reaction.stop().await.unwrap();
        assert!(server.0.lock().unwrap().attempts.is_empty());
        let saved = store.get("r", PLAN).await.unwrap();

        let replay = initialized(store.clone(), config("signal")).await;
        replay.start_with_client(server.clone()).await.unwrap();
        replay.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&replay, 1, ComponentStatus::Running).await;
        replay.stop().await.unwrap();
        assert_eq!(server.0.lock().unwrap().effects.len(), 3);
        if let Some(saved) = saved {
            assert_eq!(store.get("r", PLAN).await.unwrap().unwrap(), saved);
        }
    }
}

#[tokio::test]
async fn progress_write_failure_replays_only_unpersisted_successes() {
    for after_write in [false, true] {
        let store = Arc::new(TestStore::default());
        let server = Server::new(store.clone());
        let reaction = initialized(store.clone(), config("signal")).await;
        store.fail_write(PROGRESS, 2, after_write);
        reaction.start_with_client(server.clone()).await.unwrap();
        reaction.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&reaction, 0, ComponentStatus::Error).await;
        reaction.stop().await.unwrap();
        let saved = store.json(PLAN).await;
        assert_eq!(
            store.json(PROGRESS).await["next"],
            if after_write { 2 } else { 1 }
        );

        let replay = initialized(store.clone(), config("signal")).await;
        replay.start_with_client(server.clone()).await.unwrap();
        replay.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&replay, 1, ComponentStatus::Running).await;
        replay.stop().await.unwrap();
        let state = server.0.lock().unwrap();
        assert_eq!(state.attempts.len(), if after_write { 3 } else { 4 });
        assert_eq!(state.effects.len(), 3);
        if !after_write {
            assert_eq!(state.attempts[2].request_id, state.attempts[1].request_id);
        }
        assert_eq!(
            state.attempts.last().unwrap().request_id,
            saved["commands"][2]["request_id"]
        );
    }
}

#[tokio::test]
async fn checkpoint_write_failure_replays_no_completed_commands() {
    for after_write in [false, true] {
        let store = Arc::new(TestStore::default());
        let server = Server::new(store.clone());
        let reaction = initialized(store.clone(), config("signal")).await;
        store.fail_write(CHECKPOINT, 1, after_write);
        reaction.start_with_client(server.clone()).await.unwrap();
        reaction.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&reaction, u64::from(after_write), ComponentStatus::Error).await;
        reaction.stop().await.unwrap();
        assert_eq!(store.json(PROGRESS).await["next"], 3);

        let replay = initialized(store.clone(), config("signal")).await;
        replay.start_with_client(server.clone()).await.unwrap();
        replay.enqueue_query_result(emission(1, 3)).await.unwrap();
        // Processing the next emission proves that replay drained, even when checkpoint 1 was durable.
        replay.enqueue_query_result(emission(2, 1)).await.unwrap();
        wait_for(&replay, 2, ComponentStatus::Running).await;
        replay.stop().await.unwrap();
        assert_eq!(server.0.lock().unwrap().attempts.len(), 4);
        assert_eq!(server.0.lock().unwrap().effects.len(), 4);
        assert_eq!(store.json(PROGRESS).await["sequence"], 2);
        assert_eq!(store.json(PROGRESS).await["next"], 1);
    }
}

#[tokio::test]
async fn unfinished_delivery_rejects_changed_routing_or_query_hash() {
    for change in ["endpoint", "namespace", "mapping", "query_hash"] {
        let store = Arc::new(TestStore::default());
        let server = Server::new(store.clone());
        server.0.lock().unwrap().fail_attempt = Some((2, false));
        let reaction = initialized(store.clone(), config("signal")).await;
        reaction.start_with_client(server.clone()).await.unwrap();
        reaction.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&reaction, 0, ComponentStatus::Error).await;
        reaction.stop().await.unwrap();
        let saved = store.json(PLAN).await;
        server.0.lock().unwrap().fail_attempt = None;
        let mut altered = config("signal");
        match change {
            "endpoint" => altered.endpoint = "http://other:7233".into(),
            "namespace" => altered.namespace = "other".into(),
            "mapping" => altered = config("update"),
            "query_hash" => {}
            _ => unreachable!(),
        }
        let rejected = initialized(store.clone(), altered).await;
        if change == "query_hash" {
            rejected
                .base
                .write_checkpoint(
                    "q",
                    &ReactionCheckpoint {
                        sequence: 0,
                        config_hash: 99,
                    },
                )
                .await
                .unwrap();
        }
        rejected.start_with_client(server.clone()).await.unwrap();
        rejected.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&rejected, 0, ComponentStatus::Error).await;
        rejected.stop().await.unwrap();
        assert_eq!(server.0.lock().unwrap().attempts.len(), 2);
        assert_eq!(store.json(PLAN).await, saved);

        let replay = initialized(store.clone(), config("signal")).await;
        replay
            .base
            .write_checkpoint(
                "q",
                &ReactionCheckpoint {
                    sequence: 0,
                    config_hash: 42,
                },
            )
            .await
            .unwrap();
        replay.start_with_client(server.clone()).await.unwrap();
        replay.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&replay, 1, ComponentStatus::Running).await;
        replay.stop().await.unwrap();
        assert_eq!(server.0.lock().unwrap().effects.len(), 3);
    }
}

#[tokio::test]
async fn destructive_retry_keeps_its_run_after_rpc_or_progress_failure() {
    for action in ["cancel", "terminate"] {
        for boundary in ["rpc_before", "rpc_after", "progress"] {
            let store = Arc::new(TestStore::default());
            let server = Server::new(store.clone());
            let reaction = initialized(store.clone(), config(action)).await;
            if boundary == "progress" {
                // The first write pins the run; the second would acknowledge the accepted action.
                store.fail_write(PROGRESS, 2, false);
            } else {
                server.0.lock().unwrap().fail_attempt = Some((1, boundary == "rpc_after"));
            }
            reaction.start_with_client(server.clone()).await.unwrap();
            reaction.enqueue_query_result(emission(1, 1)).await.unwrap();
            wait_for(&reaction, 0, ComponentStatus::Error).await;
            reaction.stop().await.unwrap();
            assert_eq!(
                store.json(PROGRESS).await["resolved_run_id"],
                "original-run"
            );
            {
                let mut state = server.0.lock().unwrap();
                state.current_run = "replacement-run".into();
                state.fail_attempt = None;
            }
            let replay = initialized(store.clone(), config(action)).await;
            replay.start_with_client(server.clone()).await.unwrap();
            replay.enqueue_query_result(emission(1, 1)).await.unwrap();
            wait_for(&replay, 1, ComponentStatus::Running).await;
            replay.stop().await.unwrap();
            let state = server.0.lock().unwrap();
            assert_eq!(state.resolutions, 1);
            assert_eq!(state.attempts.len(), 2);
            assert_eq!(state.attempts[0].request_id, state.attempts[1].request_id);
            assert!(state
                .attempts
                .iter()
                .all(|c| c.resolved_run_id.as_deref() == Some("original-run")));
            assert_eq!(state.effects.len(), 1);
            assert!(!state
                .closed_runs
                .contains(&("target-0".into(), "replacement-run".into())));
        }
    }
}

#[tokio::test]
async fn failed_run_pin_write_never_sends_a_destructive_request() {
    for action in ["cancel", "terminate"] {
        for after_write in [false, true] {
            let store = Arc::new(TestStore::default());
            let server = Server::new(store.clone());
            let reaction = initialized(store.clone(), config(action)).await;
            store.fail_write(PROGRESS, 1, after_write);
            reaction.start_with_client(server.clone()).await.unwrap();
            reaction.enqueue_query_result(emission(1, 1)).await.unwrap();
            wait_for(&reaction, 0, ComponentStatus::Error).await;
            reaction.stop().await.unwrap();
            assert!(server.0.lock().unwrap().attempts.is_empty());
            server.0.lock().unwrap().current_run = "replacement-run".into();
            let replay = initialized(store.clone(), config(action)).await;
            replay.start_with_client(server.clone()).await.unwrap();
            replay.enqueue_query_result(emission(1, 1)).await.unwrap();
            wait_for(&replay, 1, ComponentStatus::Running).await;
            replay.stop().await.unwrap();
            let state = server.0.lock().unwrap();
            assert_eq!(state.attempts.len(), 1);
            assert_eq!(state.resolutions, if after_write { 1 } else { 2 });
            assert_eq!(
                state.attempts[0].resolved_run_id.as_deref(),
                Some(if after_write {
                    "original-run"
                } else {
                    "replacement-run"
                })
            );
        }
    }
}

#[tokio::test]
async fn legacy_inline_progress_migrates_without_losing_successes_or_run_pins() {
    for action in ["signal", "cancel", "terminate"] {
        for interrupted_upgrade in [false, true] {
            let store = Arc::new(TestStore::default());
            let server = Server::new(store.clone());
            let cfg = config(action);
            let qr = emission(1, 3);
            let mut commands = crate::render::commands(&cfg, &qr).unwrap();
            if action != "signal" {
                commands[1].resolved_run_id = Some("legacy-run".into());
            }
            let legacy = json!({
                "version":1, "sequence":1, "config_hash":42,
                "routing":crate::render::routing(&cfg, "q").unwrap(),
                "commands":commands, "next":1,
            });
            store
                .set("r", PLAN, serde_json::to_vec(&legacy).unwrap())
                .await
                .unwrap();
            if interrupted_upgrade {
                let interrupted = initialized(store.clone(), cfg.clone()).await;
                store.fail_write(PLAN, 1, false);
                interrupted.start_with_client(server.clone()).await.unwrap();
                interrupted.enqueue_query_result(qr.clone()).await.unwrap();
                wait_for(&interrupted, 0, ComponentStatus::Error).await;
                interrupted.stop().await.unwrap();
                assert!(server.0.lock().unwrap().attempts.is_empty());
            }
            let replay = initialized(store.clone(), cfg).await;
            replay.start_with_client(server.clone()).await.unwrap();
            replay.enqueue_query_result(qr).await.unwrap();
            wait_for(&replay, 1, ComponentStatus::Running).await;
            replay.stop().await.unwrap();
            assert_eq!(store.json(PLAN).await["version"], 2);
            let state = server.0.lock().unwrap();
            assert_eq!(state.attempts.len(), 2);
            assert_eq!(state.attempts[0].request_id, commands[1].request_id);
            assert_eq!(state.attempts[1].request_id, commands[2].request_id);
            if action != "signal" {
                assert_eq!(
                    state.attempts[0].resolved_run_id.as_deref(),
                    Some("legacy-run")
                );
                assert_eq!(state.resolutions, 1);
            }
        }
    }
}

#[tokio::test]
async fn malformed_progress_fails_without_rpc_or_checkpoint() {
    for progress in [
        json!({"sequence":1, "next":4, "resolved_run_id":null}),
        json!({"sequence":2, "next":0, "resolved_run_id":null}),
        json!({"sequence":1, "next":0, "resolved_run_id":"unexpected-run"}),
    ] {
        let store = Arc::new(TestStore::default());
        let server = Server::new(store.clone());
        server.0.lock().unwrap().fail_attempt = Some((1, false));
        let reaction = initialized(store.clone(), config("signal")).await;
        reaction.start_with_client(server.clone()).await.unwrap();
        reaction.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&reaction, 0, ComponentStatus::Error).await;
        reaction.stop().await.unwrap();
        store
            .set("r", PROGRESS, serde_json::to_vec(&progress).unwrap())
            .await
            .unwrap();
        server.0.lock().unwrap().fail_attempt = None;
        let replay = initialized(store.clone(), config("signal")).await;
        replay.start_with_client(server.clone()).await.unwrap();
        replay.enqueue_query_result(emission(1, 3)).await.unwrap();
        wait_for(&replay, 0, ComponentStatus::Error).await;
        replay.stop().await.unwrap();
        assert_eq!(server.0.lock().unwrap().attempts.len(), 1);
    }
}
