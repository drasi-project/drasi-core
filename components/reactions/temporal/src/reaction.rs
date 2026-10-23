// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_lib::{
    channels::{ComponentStatus, QueryResult},
    context::ReactionRuntimeContext,
    reactions::common::base::{ReactionBase, ReactionBaseParams},
    Reaction, ReactionRecoveryPolicy,
};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, panic::AssertUnwindSafe, sync::Arc};

use crate::{
    client,
    config::{Action, TemporalReactionConfig},
    render::{self, Command},
    TemporalReactionBuilder,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    version: u32,
    sequence: u64,
    config_hash: u64,
    routing: Value,
    commands: Vec<Command>,
    #[serde(default, rename = "next", skip_serializing)]
    legacy_next: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    sequence: u64,
    next: usize,
    resolved_run_id: Option<String>,
}

pub(crate) async fn deliver(
    base: &ReactionBase,
    client: &impl client::Transport,
    config: &TemporalReactionConfig,
    result: &QueryResult,
) -> Result<()> {
    let store = base
        .state_store()
        .await
        .context("Temporal requires a persistent state store")?;
    let checkpoint = base
        .read_checkpoint(&result.query_id)
        .await?
        .context("host must seed the query checkpoint before delivery")?;
    let key = format!("temporal:pending:{}", result.query_id);
    let progress_key = format!("temporal:progress:{}", result.query_id);
    let routing = render::routing(config, &result.query_id)?;
    let previous = store
        .get(&base.id, &key)
        .await?
        .map(|bytes| serde_json::from_slice::<Pending>(&bytes))
        .transpose()?;
    let (mut pending, is_new) = match previous {
        Some(previous) if previous.sequence > checkpoint.sequence => {
            ensure!(matches!(previous.version, 1 | 2) && previous.sequence == result.sequence
                && previous.config_hash == checkpoint.config_hash && previous.routing == routing,
                "pending Temporal actions do not match this query/configuration; restore the original configuration before replay");
            (previous, false)
        }
        _ => (
            Pending {
                version: 2,
                sequence: result.sequence,
                config_hash: checkpoint.config_hash,
                routing,
                commands: render::commands(config, result)?,
                legacy_next: 0,
            },
            true,
        ),
    };
    let previous_progress = store
        .get(&base.id, &progress_key)
        .await?
        .map(|bytes| serde_json::from_slice::<Progress>(&bytes))
        .transpose()?;
    let mut progress = match previous_progress {
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
                next: pending.legacy_next,
                resolved_run_id: pending
                    .commands
                    .get(pending.legacy_next)
                    .and_then(|command| command.resolved_run_id.clone()),
            }
        }
    };
    ensure!(
        progress.next >= pending.legacy_next && progress.next <= pending.commands.len(),
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
    if pending.version == 1 {
        // Preserve v1's inline progress before replacing it with the immutable v2 plan.
        store
            .set(&base.id, &progress_key, serde_json::to_vec(&progress)?)
            .await?;
        pending.version = 2;
        store
            .set(&base.id, &key, serde_json::to_vec(&pending)?)
            .await?;
    } else if is_new {
        store
            .set(&base.id, &key, serde_json::to_vec(&pending)?)
            .await?;
    }
    while progress.next < pending.commands.len() {
        let mut command = pending.commands[progress.next].clone();
        command.resolved_run_id = progress.resolved_run_id.clone();
        if matches!(
            command.action,
            Action::Cancel { .. } | Action::Terminate { .. }
        ) && command.resolved_run_id.is_none()
        {
            let run = match command.action.run_id() {
                Some(run) => run.to_owned(),
                None => client.resolve_run(config, &command).await?,
            };
            ensure!(
                !run.is_empty(),
                "Temporal run resolution returned an empty runId"
            );
            progress.resolved_run_id = Some(run.clone());
            // Persist the exact execution before any destructive RPC, including its retries.
            store
                .set(&base.id, &progress_key, serde_json::to_vec(&progress)?)
                .await?;
            command.resolved_run_id = Some(run);
        }
        client.send(config, &command).await?;
        progress.next += 1;
        progress.resolved_run_id = None;
        store
            .set(&base.id, &progress_key, serde_json::to_vec(&progress)?)
            .await?;
    }
    Ok(())
}

/// Future-only, durable query-diff delivery through explicitly configured Temporal actions.
pub struct TemporalReaction {
    pub(crate) base: ReactionBase,
    config: Arc<TemporalReactionConfig>,
}

impl TemporalReaction {
    pub fn builder(id: impl Into<String>) -> TemporalReactionBuilder {
        TemporalReactionBuilder::new(id)
    }

    pub(crate) fn new(
        id: String,
        queries: Vec<String>,
        config: TemporalReactionConfig,
        auto_start: bool,
    ) -> Self {
        Self {
            base: ReactionBase::new(
                ReactionBaseParams::new(id, queries)
                    .with_auto_start(auto_start)
                    .with_priority_queue_capacity(config.priority_queue_capacity),
            ),
            config: Arc::new(config),
        }
    }

    async fn start_inner(&self) -> Result<()> {
        // The host validates the real store's durability; the FFI proxy does not expose it.
        self.base
            .state_store()
            .await
            .context("Temporal requires a persistent state store")?;
        let client = client::connect(&self.config, &self.base.id).await?;
        self.start_with_client(client).await
    }

    pub(crate) async fn start_with_client(
        &self,
        client: impl client::Transport + Clone + 'static,
    ) -> Result<()> {
        let checkpoints = self.base.read_all_checkpoints().await?;
        let shutdown = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let delivery_base = Arc::new(self.base.clone_shared());
        let config = self.config.clone();
        self.base.set_status(ComponentStatus::Running, None).await;
        self.base
            .set_processing_task(tokio::spawn(async move {
                let run = base.run_standard_loop(
                    shutdown,
                    checkpoints,
                    ReactionRecoveryPolicy::Strict,
                    move |result| {
                        let client = client.clone();
                        let base = delivery_base.clone();
                        let config = config.clone();
                        async move { deliver(&base, &client, &config, &result).await }
                    },
                );
                let error = match AssertUnwindSafe(run).catch_unwind().await {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("Temporal delivery stopped: {error:#}"),
                    Err(_) => "Temporal processing panicked; checkpoint not advanced".to_owned(),
                };
                log::error!("[{}] {error}", base.id);
                base.set_status(ComponentStatus::Error, Some(error)).await;
            }))
            .await;
        Ok(())
    }
}

#[async_trait]
impl Reaction for TemporalReaction {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "temporal"
    }
    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }
    fn properties(&self) -> HashMap<String, Value> {
        self.base
            .properties_or_serialize(&crate::descriptor::TemporalReactionConfigDto::from(
                self.config.as_ref(),
            ))
    }
    fn is_durable(&self) -> bool {
        true
    }
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }
    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
    }
    async fn start(&self) -> Result<()> {
        ensure!(
            !matches!(
                self.status().await,
                ComponentStatus::Running | ComponentStatus::Starting
            ),
            "Temporal reaction is already started"
        );
        self.base.set_status(ComponentStatus::Starting, None).await;
        if let Err(error) = self.start_inner().await {
            self.base
                .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                .await;
            return Err(error);
        }
        Ok(())
    }
}
