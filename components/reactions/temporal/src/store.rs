// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{ensure, Result};
use drasi_lib::{channels::QueryResult, ReactionCheckpoint, StateStoreProvider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::{Action, TemporalReactionConfig},
    render::{self, Command},
};

const PLAN_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pending {
    version: u32,
    sequence: u64,
    config_hash: u64,
    routing: Value,
    pub commands: Vec<Command>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    sequence: u64,
    pub next: usize,
    pub resolved_run_id: Option<String>,
}

pub(crate) struct PlanStore<'a> {
    store: &'a dyn StateStoreProvider,
    reaction_id: &'a str,
    pending_key: String,
    progress_key: String,
}

impl<'a> PlanStore<'a> {
    pub fn new(store: &'a dyn StateStoreProvider, reaction_id: &'a str, query_id: &str) -> Self {
        Self {
            store,
            reaction_id,
            pending_key: format!("temporal:pending:{query_id}"),
            progress_key: format!("temporal:progress:{query_id}"),
        }
    }

    async fn load_pending(&self) -> Result<Option<Pending>> {
        let pending = self
            .store
            .get(self.reaction_id, &self.pending_key)
            .await?
            .map(|bytes| serde_json::from_slice::<Pending>(&bytes))
            .transpose()?;
        if let Some(pending) = &pending {
            ensure!(
                pending.version == PLAN_VERSION,
                "unsupported Temporal plan version {}",
                pending.version
            );
        }
        Ok(pending)
    }

    async fn save_pending(&self, pending: &Pending) -> Result<()> {
        self.store
            .set(
                self.reaction_id,
                &self.pending_key,
                serde_json::to_vec(pending)?,
            )
            .await?;
        Ok(())
    }

    async fn load_progress(&self) -> Result<Option<Progress>> {
        Ok(self
            .store
            .get(self.reaction_id, &self.progress_key)
            .await?
            .map(|bytes| serde_json::from_slice::<Progress>(&bytes))
            .transpose()?)
    }

    pub async fn save_progress(&self, progress: &Progress) -> Result<()> {
        self.store
            .set(
                self.reaction_id,
                &self.progress_key,
                serde_json::to_vec(progress)?,
            )
            .await?;
        Ok(())
    }

    pub async fn prepare(
        &self,
        config: &TemporalReactionConfig,
        result: &QueryResult,
        checkpoint: &ReactionCheckpoint,
    ) -> Result<(Pending, Progress)> {
        let routing = render::routing(config, &result.query_id)?;
        let (pending, is_new) = match self.load_pending().await? {
            Some(previous) if previous.sequence > checkpoint.sequence => {
                ensure!(previous.sequence == result.sequence
                    && previous.config_hash == checkpoint.config_hash && previous.routing == routing,
                    "pending Temporal actions do not match this query/configuration; restore the original configuration before replay");
                (previous, false)
            }
            _ => (
                Pending {
                    version: PLAN_VERSION,
                    sequence: result.sequence,
                    config_hash: checkpoint.config_hash,
                    routing,
                    commands: render::commands(config, result)?,
                },
                true,
            ),
        };
        let progress = match self.load_progress().await? {
            Some(progress) if progress.sequence == pending.sequence => {
                ensure!(!is_new, "Temporal progress has no matching pending plan");
                progress
            }
            previous => {
                ensure!(
                    previous.is_none_or(|p| p.sequence < pending.sequence),
                    "Temporal progress is newer than its pending plan"
                );
                Progress {
                    sequence: pending.sequence,
                    next: 0,
                    resolved_run_id: None,
                }
            }
        };
        ensure!(
            progress.next <= pending.commands.len(),
            "invalid pending Temporal progress"
        );
        if let Some(run) = &progress.resolved_run_id {
            ensure!(
                !run.is_empty()
                    && pending
                        .commands
                        .get(progress.next)
                        .is_some_and(|command| matches!(
                            command.action,
                            Action::Cancel { .. } | Action::Terminate { .. }
                        )),
                "invalid pending Temporal run resolution"
            );
        }
        if is_new {
            self.save_pending(&pending).await?;
        }
        Ok((pending, progress))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::{channels::ResultDiff, MemoryStateStoreProvider};
    use serde_json::json;

    fn inputs() -> (TemporalReactionConfig, QueryResult, ReactionCheckpoint) {
        let config = serde_json::from_value(json!({
            "outputTemplates":{"routes":{"q":{"added":{
                "action":"cancel", "workflowId":"{{after.id}}"
            }}}}
        }))
        .unwrap();
        let result = QueryResult {
            query_id: "q".into(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            results: vec![ResultDiff::Add {
                data: json!({"id":"target"}),
                row_signature: 1,
            }],
            metadata: Default::default(),
            profiling: None,
        };
        (
            config,
            result,
            ReactionCheckpoint {
                sequence: 0,
                config_hash: 42,
            },
        )
    }

    #[tokio::test]
    async fn prepare_replays_frozen_plan_and_progress_without_transport() {
        let memory = MemoryStateStoreProvider::new();
        let store = PlanStore::new(&memory, "r", "q");
        let (config, mut result, mut checkpoint) = inputs();
        let (pending, mut progress) = store.prepare(&config, &result, &checkpoint).await.unwrap();
        let bytes = memory.get("r", "temporal:pending:q").await.unwrap();
        progress.resolved_run_id = Some("pinned-run".into());
        store.save_progress(&progress).await.unwrap();
        result.results.clear();
        let (replayed, progress) = store.prepare(&config, &result, &checkpoint).await.unwrap();
        assert_eq!(
            replayed.commands[0].request_id,
            pending.commands[0].request_id
        );
        assert_eq!(progress.resolved_run_id.as_deref(), Some("pinned-run"));
        assert_eq!(memory.get("r", "temporal:pending:q").await.unwrap(), bytes);

        checkpoint.sequence = 1;
        result.sequence = 2;
        let (next, progress) = store.prepare(&config, &result, &checkpoint).await.unwrap();
        assert!(next.commands.is_empty());
        assert_eq!(progress.sequence, 2);
        assert_eq!(progress.next, 0);
        assert!(progress.resolved_run_id.is_none());
    }

    #[tokio::test]
    async fn progress_without_matching_plan_does_not_overwrite_state() {
        let memory = MemoryStateStoreProvider::new();
        let store = PlanStore::new(&memory, "r", "q");
        let (config, result, checkpoint) = inputs();
        store
            .save_progress(&Progress {
                sequence: 1,
                next: 0,
                resolved_run_id: None,
            })
            .await
            .unwrap();
        let error = store
            .prepare(&config, &result, &checkpoint)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("no matching pending plan"));
        assert!(memory
            .get("r", "temporal:pending:q")
            .await
            .unwrap()
            .is_none());
    }
}
