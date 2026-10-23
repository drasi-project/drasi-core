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
use temporalio_client::Client;

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
    next: usize,
}

pub(crate) async fn deliver(
    base: &ReactionBase,
    client: &Client,
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
    let routing = render::routing(config, &result.query_id)?;
    let previous = store
        .get(&base.id, &key)
        .await?
        .map(|bytes| serde_json::from_slice::<Pending>(&bytes))
        .transpose()?;
    let mut pending = match previous {
        Some(previous) if previous.sequence > checkpoint.sequence => {
            ensure!(previous.version == 1 && previous.sequence == result.sequence
                && previous.config_hash == checkpoint.config_hash && previous.routing == routing,
                "pending Temporal actions do not match this query/configuration; restore the original configuration before replay");
            ensure!(
                previous.next <= previous.commands.len(),
                "invalid pending Temporal progress"
            );
            previous
        }
        _ => Pending {
            version: 1,
            sequence: result.sequence,
            config_hash: checkpoint.config_hash,
            routing,
            commands: render::commands(config, result)?,
            next: 0,
        },
    };
    store
        .set(&base.id, &key, serde_json::to_vec(&pending)?)
        .await?;
    while pending.next < pending.commands.len() {
        let command = &mut pending.commands[pending.next];
        if matches!(
            command.action,
            Action::Cancel { .. } | Action::Terminate { .. }
        ) && command.resolved_run_id.is_none()
        {
            command.resolved_run_id = Some(match command.action.run_id() {
                Some(run) => run.to_owned(),
                None => client::resolve_run(client, config, command).await?,
            });
            // Persist the exact execution before any destructive RPC, including its retries.
            store
                .set(&base.id, &key, serde_json::to_vec(&pending)?)
                .await?;
        }
        client::send(client, config, &pending.commands[pending.next]).await?;
        pending.next += 1;
        store
            .set(&base.id, &key, serde_json::to_vec(&pending)?)
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
        let checkpoints = self.base.read_all_checkpoints().await?;
        let client = client::connect(&self.config, &self.base.id).await?;
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
