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

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::{
    ManagerCheckpointOwnership, ReactionBase, ReactionBaseParams, ReactionCheckpoint,
};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::{DrasiLib, Query, Reaction};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use tokio::sync::mpsc;

const SOURCE_ID: &str = "people-source";
const QUERY_ID: &str = "people-query";
const REACTION_ID: &str = "durable-recorder";

struct DurableRecordingReaction {
    base: ReactionBase,
    delivered: mpsc::UnboundedSender<QueryResult>,
    recovery_policy: ReactionRecoveryPolicy,
}

impl DurableRecordingReaction {
    fn new(
        recovery_policy: ReactionRecoveryPolicy,
    ) -> (Self, mpsc::UnboundedReceiver<QueryResult>) {
        let (delivered, receiver) = mpsc::unbounded_channel();
        (
            Self {
                base: ReactionBase::new(ReactionBaseParams::new(
                    REACTION_ID,
                    vec![QUERY_ID.to_string()],
                )),
                delivered,
                recovery_policy,
            },
            receiver,
        )
    }
}

#[async_trait]
impl Reaction for DurableRecordingReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "durable-recording"
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

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        let checkpoint = self
            .base
            .read_checkpoint(&result.query_id)
            .await?
            .context("host must seed the reaction checkpoint before live delivery")?;
        self.base
            .write_checkpoint(
                &result.query_id,
                &ReactionCheckpoint {
                    sequence: result.sequence,
                    config_hash: checkpoint.config_hash,
                },
            )
            .await?;
        self.delivered
            .send(result)
            .map_err(|_| anyhow::anyhow!("recording receiver closed"))
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.recovery_policy
    }

    fn checkpoint_ownership(&self) -> ManagerCheckpointOwnership {
        ManagerCheckpointOwnership::Reaction
    }
}

async fn build_run(
    database_path: &Path,
    query_text: &str,
    recovery_policy: ReactionRecoveryPolicy,
) -> Result<(
    DrasiLib,
    ApplicationSourceHandle,
    mpsc::UnboundedReceiver<QueryResult>,
)> {
    let (source, source_handle) = ApplicationSource::new(
        SOURCE_ID,
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )?;
    let (reaction, receiver) = DurableRecordingReaction::new(recovery_policy);
    let state_store = Arc::new(RedbStateStoreProvider::new(database_path)?);

    let core = DrasiLib::builder()
        .with_id("redb-restart-sequence")
        .with_state_store_provider(state_store)
        .with_source(source)
        .with_query(
            Query::cypher(QUERY_ID)
                .query(query_text)
                .from_source(SOURCE_ID)
                .auto_start(true)
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;
    Ok((core, source_handle, receiver))
}

async fn insert_person(
    source: &ApplicationSourceHandle,
    id: &str,
    name: &str,
    age: i64,
) -> Result<()> {
    source
        .send_node_insert(
            id,
            vec!["Person"],
            PropertyMapBuilder::new()
                .with_string("name", name)
                .with_integer("age", age)
                .build(),
        )
        .await
}

async fn receive_one(receiver: &mut mpsc::UnboundedReceiver<QueryResult>) -> QueryResult {
    tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("timed out waiting for reaction delivery")
        .expect("reaction delivery channel closed")
}

async fn assert_no_delivery(receiver: &mut mpsc::UnboundedReceiver<QueryResult>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(300), receiver.recv())
            .await
            .is_err(),
        "an already-acknowledged result was delivered again"
    );
}

async fn stop_reaction_and_wait(core: &DrasiLib) -> Result<()> {
    core.stop_reaction(REACTION_ID).await?;
    for _ in 0..100 {
        let status = core
            .list_reactions()
            .await?
            .into_iter()
            .find(|(id, _)| id == REACTION_ID)
            .map(|(_, status)| status);
        if status == Some(ComponentStatus::Stopped) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("reaction did not stop")
}

#[tokio::test]
async fn durable_reaction_checkpoint_worker() -> Result<()> {
    let Some(phase) = std::env::var_os("DRASI_REDB_RESTART_PHASE") else {
        return Ok(());
    };
    let database_path = std::env::var_os("DRASI_REDB_RESTART_PATH")
        .map(std::path::PathBuf::from)
        .context("DRASI_REDB_RESTART_PATH must be set for restart worker")?;
    let original_query = "MATCH (p:Person) RETURN p.name AS name, p.age AS age";

    match phase.to_str().context("restart phase must be UTF-8")? {
        "seed" => {
            let (core, source, mut receiver) = build_run(
                &database_path,
                original_query,
                ReactionRecoveryPolicy::Strict,
            )
            .await?;
            insert_person(&source, "p1", "Alice", 30).await?;
            assert_eq!(receive_one(&mut receiver).await.sequence, 1);
            core.shutdown().await?;
        }
        "resume" => {
            // Every runtime object and the process-local query counter are new.
            // The matching redb checkpoint must restore the sequence floor.
            let (core, source, mut receiver) = build_run(
                &database_path,
                original_query,
                ReactionRecoveryPolicy::Strict,
            )
            .await?;
            assert_no_delivery(&mut receiver).await;

            insert_person(&source, "p2", "Bob", 31).await?;
            let second = receive_one(&mut receiver).await;
            assert_eq!(second.sequence, 2);
            assert_eq!(second.results.len(), 1);
            assert_no_delivery(&mut receiver).await;

            // Rewiring against the same live query must suppress the
            // already-acknowledged sequence instead of replaying it.
            stop_reaction_and_wait(&core).await?;
            core.start_reaction(REACTION_ID).await?;
            assert_no_delivery(&mut receiver).await;

            insert_person(&source, "p3", "Carol", 32).await?;
            assert_eq!(receive_one(&mut receiver).await.sequence, 3);
            assert_no_delivery(&mut receiver).await;
            core.shutdown().await?;
        }
        "strict-config-change" => {
            let changed_query =
                "MATCH (p:Person) WHERE p.age >= 0 RETURN p.name AS name, p.age AS age";
            let result = build_run(
                &database_path,
                changed_query,
                ReactionRecoveryPolicy::Strict,
            )
            .await;
            let error = match result {
                Ok(_) => anyhow::bail!("Strict recovery accepted a mismatched config hash"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("strict recovery"),
                "unexpected Strict recovery error: {error:#}"
            );
        }
        "config-change" => {
            // AutoReset ignores the mismatched reaction checkpoint and
            // snapshots the changed query at its restart-stable global clock.
            let changed_query =
                "MATCH (p:Person) WHERE p.age >= 0 RETURN p.name AS name, p.age AS age";
            let (core, source, mut receiver) = build_run(
                &database_path,
                changed_query,
                ReactionRecoveryPolicy::AutoReset,
            )
            .await?;
            assert_no_delivery(&mut receiver).await;

            insert_person(&source, "p4", "Diana", 33).await?;
            assert_eq!(receive_one(&mut receiver).await.sequence, 4);
            assert_no_delivery(&mut receiver).await;
            core.shutdown().await?;
        }
        unknown => anyhow::bail!("unknown restart worker phase: {unknown}"),
    }

    Ok(())
}

fn run_restart_phase(database_path: &Path, phase: &str) {
    let output = Command::new(std::env::current_exe().expect("test executable path"))
        .args([
            "--exact",
            "durable_reaction_checkpoint_worker",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DRASI_REDB_RESTART_PATH", database_path)
        .env("DRASI_REDB_RESTART_PHASE", phase)
        .output()
        .expect("restart worker must launch");

    assert!(
        output.status.success(),
        "restart phase '{phase}' failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn durable_reaction_checkpoint_restores_query_sequence_across_process_restart() {
    let temp_dir = tempfile::tempdir().expect("temporary redb directory");
    let database_path = temp_dir.path().join("reaction-state.redb");

    // Each phase runs in a fresh process and reopens the exact same redb file.
    run_restart_phase(&database_path, "seed");
    run_restart_phase(&database_path, "resume");
    run_restart_phase(&database_path, "strict-config-change");
    run_restart_phase(&database_path, "config-change");
}
