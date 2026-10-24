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

#![allow(dead_code)]

use async_trait::async_trait;
use drasi_lib::{
    channels::QueryResult, reactions::BootstrapContext, ComponentStatus, Reaction, ReactionBase,
    ReactionBaseParams, ReactionCheckpoint, ReactionRecoveryPolicy, ReactionRuntimeContext,
    StateStoreProvider,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

pub const DEADLINE: Duration = Duration::from_secs(15);

pub struct Gate {
    pub entered: Notify,
    pub release: Semaphore,
}

impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            release: Semaphore::new(0),
        })
    }

    async fn wait(&self) {
        self.entered.notify_one();
        self.release.acquire().await.expect("gate open").forget();
    }
}

#[derive(Default)]
pub struct Probe {
    pub starts: AtomicUsize,
    pub accepted: Mutex<Vec<QueryResult>>,
    pub attempts: Mutex<Vec<(String, u64)>>,
    pub handled: Mutex<Vec<QueryResult>>,
    pub snapshots: Mutex<Vec<(String, bool, u64, Vec<serde_json::Value>)>>,
    pub fail_effect: AtomicBool,
    pub fail_bootstrap: AtomicBool,
}

#[derive(Clone)]
pub struct ProbeReaction {
    pub base: Arc<ReactionBase>,
    pub probe: Arc<Probe>,
    pub start_gate: Option<Arc<Gate>>,
    pub bootstrap_gate: Option<Arc<Gate>>,
    pub handles_results: bool,
    pub durable: bool,
    pub snapshot: bool,
    pub policy: ReactionRecoveryPolicy,
}

impl ProbeReaction {
    pub fn new(id: &str, queries: &[&str]) -> Self {
        Self {
            base: Arc::new(ReactionBase::new(
                ReactionBaseParams::new(
                    id,
                    queries.iter().map(|query| (*query).to_owned()).collect(),
                )
                .with_auto_start(false),
            )),
            probe: Arc::new(Probe::default()),
            start_gate: None,
            bootstrap_gate: None,
            handles_results: false,
            durable: false,
            snapshot: false,
            policy: ReactionRecoveryPolicy::Strict,
        }
    }
}

#[async_trait]
impl Reaction for ProbeReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "computation-recovery-probe"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.get_queries().to_vec()
    }

    fn auto_start(&self) -> bool {
        false
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

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        let start = self.probe.starts.fetch_add(1, Ordering::SeqCst);
        self.base.set_status(ComponentStatus::Starting, None).await;
        if start == 0 {
            if let Some(gate) = &self.start_gate {
                gate.wait().await;
            }
        }
        if self.handles_results {
            let shutdown = self.base.create_shutdown_channel().await;
            let checkpoints = self.base.read_all_checkpoints().await?;
            let base = self.base.clone_shared();
            let probe = self.probe.clone();
            let policy = self.policy;
            let task = tokio::spawn(async move {
                let _ = base
                    .run_standard_loop(shutdown, checkpoints, policy, move |result| {
                        let probe = probe.clone();
                        async move {
                            probe
                                .attempts
                                .lock()
                                .unwrap()
                                .push((result.query_id.clone(), result.sequence));
                            if probe.fail_effect.swap(false, Ordering::SeqCst) {
                                anyhow::bail!("injected side-effect failure");
                            }
                            probe.handled.lock().unwrap().push(result.as_ref().clone());
                            Ok(())
                        }
                    })
                    .await;
            });
            self.base.set_processing_task(task).await;
        }
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result.clone()).await?;
        self.probe.accepted.lock().unwrap().push(result);
        Ok(())
    }

    async fn bootstrap(&self, context: BootstrapContext) -> anyhow::Result<()> {
        let snapshot = context.fetch_snapshot().await?;
        let checkpoint = ReactionCheckpoint {
            sequence: snapshot.as_of_sequence,
            config_hash: snapshot.config_hash,
        };
        let rows = snapshot.collect_vec().await;
        let first = {
            let mut snapshots = self.probe.snapshots.lock().unwrap();
            let first = snapshots.is_empty();
            snapshots.push((
                context.query_id.clone(),
                context.is_reset,
                checkpoint.sequence,
                rows,
            ));
            first
        };
        context.write_checkpoint(&checkpoint).await?;
        if first {
            if let Some(gate) = &self.bootstrap_gate {
                gate.wait().await;
            }
        }
        if self.probe.fail_bootstrap.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected bootstrap failure after checkpoint staging");
        }
        Ok(())
    }
}

pub async fn checkpoint(
    store: &dyn StateStoreProvider,
    reaction: &str,
    query: &str,
) -> Option<ReactionCheckpoint> {
    store
        .get(reaction, &format!("checkpoint:{query}"))
        .await
        .expect("read checkpoint")
        .map(|bytes| bincode::deserialize(&bytes).expect("checkpoint payload"))
}

pub async fn wait_checkpoint(
    store: &dyn StateStoreProvider,
    reaction: &str,
    query: &str,
    sequence: u64,
) -> ReactionCheckpoint {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(checkpoint) = checkpoint(store, reaction, query).await {
                if checkpoint.sequence == sequence {
                    return checkpoint;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("checkpoint deadline")
}

pub async fn wait_until(condition: impl Fn() -> bool) {
    tokio::time::timeout(DEADLINE, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("reaction observation deadline");
}
