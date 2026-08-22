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

use crate::config::GitHubWorkGraphDispatcherConfig;
use crate::dispatcher::{Clock, DispatcherEngine, LeaseIdGenerator, SystemClock, UuidV7Generator};
use crate::github::{GitHubApi, RestGitHubApi};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::reactions::checkpoint::ReactionCheckpoint;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::BootstrapContext;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::Reaction;
use log::{error, info};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) const INBOX_PREFIX: &str = "github-workgraph-dispatcher/inbox/";

pub struct GitHubWorkGraphDispatcher {
    pub(crate) base: ReactionBase,
    pub(crate) config: GitHubWorkGraphDispatcherConfig,
    clock: Arc<dyn Clock>,
    lease_ids: Arc<dyn LeaseIdGenerator>,
    engine: Arc<Mutex<Option<DispatcherEngine>>>,
    ingest_failure: Arc<Mutex<Option<String>>>,
}

impl GitHubWorkGraphDispatcher {
    pub fn builder(id: impl Into<String>) -> crate::GitHubWorkGraphDispatcherBuilder {
        crate::GitHubWorkGraphDispatcherBuilder::new(id)
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: GitHubWorkGraphDispatcherConfig,
        auto_start: bool,
    ) -> Self {
        let params = ReactionBaseParams::new(id, queries)
            .with_auto_start(auto_start)
            .with_priority_queue_capacity(config.priority_queue_capacity)
            .with_recovery_policy(ReactionRecoveryPolicy::Strict);
        Self {
            base: ReactionBase::new(params),
            config,
            clock: Arc::new(SystemClock),
            lease_ids: Arc::new(UuidV7Generator),
            engine: Arc::new(Mutex::new(None)),
            ingest_failure: Arc::new(Mutex::new(None)),
        }
    }

    async fn fail_start(&self, error: anyhow::Error) -> anyhow::Error {
        self.base
            .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
            .await;
        error
    }

    async fn build_engine(&self) -> Result<DispatcherEngine> {
        let state_store = self
            .base
            .state_store()
            .await
            .context("github-workgraph-dispatcher requires a durable state store")?;
        anyhow::ensure!(
            state_store.is_durable(),
            "github-workgraph-dispatcher requires a durable state store"
        );
        let github: Arc<dyn GitHubApi> = Arc::new(RestGitHubApi::new(&self.config)?);
        Ok(DispatcherEngine::new(
            self.base.id.clone(),
            self.base.queries[0].clone(),
            self.config.clone(),
            state_store,
            github,
            Arc::clone(&self.clock),
            Arc::clone(&self.lease_ids),
        ))
    }

    fn inbox_key(result: &QueryResult) -> String {
        let query_hash = hex::encode(Sha256::digest(result.query_id.as_bytes()));
        format!("{INBOX_PREFIX}{query_hash}/{:020}", result.sequence)
    }

    async fn persist_inbox(&self, result: &QueryResult) -> Result<()> {
        ensure!(
            result.query_id == self.base.queries[0],
            "received result from unexpected query '{}'",
            result.query_id
        );
        let store = self
            .base
            .state_store()
            .await
            .context("github-workgraph-dispatcher requires a durable state store")?;
        ensure!(
            store.is_durable(),
            "github-workgraph-dispatcher requires a durable state store"
        );
        let key = Self::inbox_key(result);
        let value = serde_json::to_value(result).context("failed to serialize dispatcher inbox")?;
        if let Some(existing) = store
            .get(&self.base.id, &key)
            .await
            .context("failed to read dispatcher inbox")?
        {
            let existing: serde_json::Value =
                serde_json::from_slice(&existing).context("dispatcher inbox record is corrupt")?;
            ensure!(
                existing == value,
                "query sequence {} was enqueued with conflicting content",
                result.sequence
            );
            return Ok(());
        }
        store
            .set(
                &self.base.id,
                &key,
                serde_json::to_vec(&value).context("failed to encode dispatcher inbox")?,
            )
            .await
            .context("failed to persist dispatcher inbox")?;
        store
            .sync()
            .await
            .context("failed to sync dispatcher inbox")
    }

    async fn delete_inbox(base: &ReactionBase, result: &QueryResult) -> Result<()> {
        let store = base
            .state_store()
            .await
            .context("github-workgraph-dispatcher requires a durable state store")?;
        store
            .delete(&base.id, &Self::inbox_key(result))
            .await
            .context("failed to delete dispatcher inbox record")?;
        store
            .sync()
            .await
            .context("failed to sync dispatcher inbox deletion")
    }

    async fn process_durable_event(
        base: &ReactionBase,
        engine: &mut DispatcherEngine,
        result: &QueryResult,
    ) -> Result<()> {
        engine.process(result).await?;
        if let Some(checkpoint) = base.read_checkpoint(&result.query_id).await? {
            if result.sequence > checkpoint.sequence {
                base.write_checkpoint(
                    &result.query_id,
                    &ReactionCheckpoint {
                        sequence: result.sequence,
                        config_hash: checkpoint.config_hash,
                    },
                )
                .await?;
            }
        }
        Self::delete_inbox(base, result).await
    }

    async fn recover_inbox(&self, engine: &mut DispatcherEngine) -> Result<()> {
        let store = self
            .base
            .state_store()
            .await
            .context("github-workgraph-dispatcher requires a durable state store")?;
        let mut records = Vec::new();
        for key in store
            .list_keys(&self.base.id)
            .await
            .context("failed to list dispatcher inbox")?
        {
            if !key.starts_with(INBOX_PREFIX) {
                continue;
            }
            let bytes = store
                .get(&self.base.id, &key)
                .await
                .context("failed to read dispatcher inbox")?
                .with_context(|| format!("dispatcher inbox key '{key}' disappeared"))?;
            let result: QueryResult = serde_json::from_slice(&bytes)
                .with_context(|| format!("dispatcher inbox record '{key}' is corrupt"))?;
            ensure!(
                result.query_id == self.base.queries[0] && Self::inbox_key(&result) == key,
                "dispatcher inbox record '{key}' has a corrupt identity"
            );
            records.push(result);
        }
        records.sort_by_key(|result| result.sequence);
        for result in records {
            Self::process_durable_event(&self.base, engine, &result).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Reaction for GitHubWorkGraphDispatcher {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "github-workgraph-dispatcher"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        *self.ingest_failure.lock().await = None;
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Recovering durable WorkGraph dispatcher ledger".to_string()),
            )
            .await;
        if let Err(error) = self.config.validate(&self.base.queries) {
            return Err(self.fail_start(error).await);
        }
        let mut engine = match self.build_engine().await {
            Ok(engine) => engine,
            Err(error) => return Err(self.fail_start(error).await),
        };
        if let Err(error) = engine.recover().await {
            return Err(self.fail_start(error).await);
        }
        if let Err(error) = self.recover_inbox(&mut engine).await {
            return Err(self.fail_start(error).await);
        }
        *self.engine.lock().await = Some(engine);

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let status = self.base.status_handle();
        let reaction_id = self.base.id.clone();
        let engine = Arc::clone(&self.engine);
        let processing_task = tokio::spawn(async move {
            info!("[{reaction_id}] WorkGraph dispatcher loop started");
            loop {
                let event = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    event = base.priority_queue.dequeue() => event,
                };
                let result = {
                    let mut engine = engine.lock().await;
                    match engine.as_mut() {
                        Some(engine) => Self::process_durable_event(&base, engine, &event).await,
                        None => Err(anyhow::anyhow!("dispatcher engine is not initialized")),
                    }
                };
                if let Err(error) = result {
                    error!("[{reaction_id}] dispatcher fail-stop: {error:#}");
                    status
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("WorkGraph dispatcher fail-stop: {error:#}")),
                        )
                        .await;
                    break;
                }
            }
        });
        self.base.set_processing_task(processing_task).await;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("WorkGraph dispatcher recovered and running".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: drasi_lib::channels::QueryResult) -> Result<()> {
        let mut ingest_failure = self.ingest_failure.lock().await;
        if let Some(reason) = ingest_failure.as_ref() {
            anyhow::bail!("dispatcher input is fail-stopped after an inbox error: {reason}");
        }
        if let Err(error) = self.persist_inbox(&result).await {
            let reason = format!("{error:#}");
            *ingest_failure = Some(reason.clone());
            drop(ingest_failure);
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Dispatcher durable inbox failed: {reason}")),
                )
                .await;
            return Err(error);
        }
        drop(ingest_failure);
        self.base.enqueue_query_result(result).await
    }

    async fn bootstrap(&self, ctx: BootstrapContext) -> Result<()> {
        ensure!(
            ctx.query_id == self.base.queries[0],
            "received bootstrap for unexpected query '{}'",
            ctx.query_id
        );
        let mut snapshot = ctx
            .fetch_snapshot()
            .await
            .map_err(|error| anyhow::anyhow!("failed to fetch capacity snapshot: {error}"))?;
        let sequence = snapshot.as_of_sequence;
        let mut results = Vec::new();
        while let Some((row_signature, data)) = snapshot.next_keyed().await {
            results.push(ResultDiff::Add {
                data,
                row_signature,
            });
        }
        let event = QueryResult::new(
            ctx.query_id,
            sequence,
            chrono::Utc::now(),
            results,
            HashMap::new(),
        );
        let mut engine = self.engine.lock().await;
        let engine = engine
            .as_mut()
            .context("dispatcher engine is not initialized during bootstrap")?;
        engine.process(&event).await?;
        engine.persist_bootstrap_watermark(sequence).await
    }

    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }
}
