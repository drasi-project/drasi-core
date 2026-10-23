// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashMap, sync::Arc};

use anyhow::{ensure, Result};
use async_trait::async_trait;
use drasi_lib::{
    channels::{ComponentStatus, QueryResult},
    context::ReactionRuntimeContext,
    identity::IdentityProvider,
    reactions::common::base::{ReactionBase, ReactionBaseParams},
    recovery::ReactionRecoveryPolicy,
    Reaction,
};
use serde_json::Value;

use crate::{
    client::DtsClient, descriptor::DurableTaskReactionConfigDto, dispatch,
    DurableTaskReactionBuilder, DurableTaskReactionConfig,
};

/// Ordered, checkpointed dispatch to Durable Task Scheduler.
pub struct DurableTaskReaction {
    pub(crate) base: ReactionBase,
    config: DurableTaskReactionConfig,
    capacity: Option<usize>,
}

impl DurableTaskReaction {
    pub fn builder(id: impl Into<String>) -> DurableTaskReactionBuilder {
        DurableTaskReactionBuilder::new(id)
    }

    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: DurableTaskReactionConfig,
    ) -> Result<Self> {
        Self::builder(id)
            .with_queries(queries)
            .with_config(config)
            .build()
    }

    pub(crate) fn from_builder(
        params: ReactionBaseParams,
        config: DurableTaskReactionConfig,
    ) -> Self {
        Self {
            capacity: params.priority_queue_capacity,
            base: ReactionBase::new(params),
            config,
        }
    }

    async fn start_inner(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Starting, None).await;
        let store = self.base.state_store().await;
        ensure!(
            store.is_some_and(|store| store.is_durable()),
            "DTS reaction requires a persistent state store"
        );
        let client = DtsClient::new(&self.config, self.base.identity_provider().await)?;
        let initial = self.base.read_all_checkpoints().await?;
        let mut shutdown = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let config = Arc::new(self.config.clone());
        let registry = Arc::new(dispatch::handlebars());
        let id = self.base.id.clone();
        let policy = self.default_recovery_policy();
        let ready = Arc::new(tokio::sync::Notify::new());
        let started = ready.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = &mut shutdown => return,
                _ = started.notified() => {}
            }
            let result = base
                .run_standard_loop(shutdown, initial, policy, |emission| {
                    let mut client = client.clone();
                    let config = config.clone();
                    let registry = registry.clone();
                    let id = id.clone();
                    async move {
                        let operations = dispatch::prepare(&config, &id, &emission, &registry)?;
                        for operation in operations {
                            client.dispatch(&operation).await?;
                        }
                        Ok(())
                    }
                })
                .await;
            if let Err(error) = result {
                log::error!("[{}] Durable Task reaction stopped: {error:#}", base.id);
                base.set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                    .await;
            }
        });
        self.base.set_processing_task(handle).await;
        self.base.set_status(ComponentStatus::Running, None).await;
        ready.notify_one();
        Ok(())
    }
}

#[async_trait]
impl Reaction for DurableTaskReaction {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "durable-task"
    }
    fn properties(&self) -> HashMap<String, Value> {
        let mut dto = DurableTaskReactionConfigDto::from(&self.config);
        dto.priority_queue_capacity = self
            .capacity
            .map(|value| drasi_plugin_sdk::config_value::ConfigValue::Static(value as u64));
        dto.recovery_policy = self.base.recovery_policy.map(Into::into);
        self.base.properties_or_serialize(&dto)
    }
    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }
    fn is_durable(&self) -> bool {
        true
    }
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.base
            .recovery_policy
            .unwrap_or(ReactionRecoveryPolicy::Strict)
    }
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }
    async fn set_identity_provider(&self, provider: Arc<dyn IdentityProvider>) {
        self.base.set_identity_provider(provider).await;
    }
    async fn start(&self) -> Result<()> {
        ensure!(
            !matches!(
                self.base.get_status().await,
                ComponentStatus::Running | ComponentStatus::Starting
            ),
            "reaction is already running"
        );
        if let Err(error) = self.start_inner().await {
            self.base
                .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                .await;
            return Err(error);
        }
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
    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
    }
}
