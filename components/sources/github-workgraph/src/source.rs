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

use crate::agent_sync::AgentSync;
use crate::config::{GitHubWorkGraphSourceConfig, RepositoryFilter};
use crate::lease_ledger::Allocator;
use crate::mapping::{NODE_LABELS, RELATION_LABELS};
use crate::webhook::{serve, IngressParams};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::channels::events::{SourceEvent, SourceEventWrapper};
use drasi_lib::channels::{ComponentStatus, DispatchMode, SubscriptionResponse};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::schema::{NodeSchema, RelationSchema, SourceSchema};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::{ByteLexPositionComparator, SourceError};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::WalProvider;
use drasi_lib::Source;
use log::{error, info, warn};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::{timeout, Duration, Instant};

const SOURCE_TYPE: &str = "github-workgraph";
const DRAIN_TICK: Duration = Duration::from_millis(500);
const TASK_GRACE: Duration = Duration::from_secs(3);
// DrasiLib releases this fence through the lifecycle hook. The fallback only
// prevents unbounded retention in standalone hosts that omit that hook.
const PRUNING_FENCE_FALLBACK: Duration = Duration::from_secs(60);

async fn report(base: &SourceBase, status: ComponentStatus, message: &str) {
    base.set_status(status, Some(message.to_string())).await;
}

pub struct GitHubWorkGraphSource {
    pub(crate) base: SourceBase,
    config: GitHubWorkGraphSourceConfig,
    repository_filter: RepositoryFilter,
    wal: Arc<RwLock<Option<Arc<dyn WalProvider>>>>,
    allocator: Arc<RwLock<Option<Arc<Allocator>>>>,
    agent_sync: Arc<RwLock<Option<Arc<AgentSync>>>>,
    notify: Arc<Notify>,
    replay_gate: Arc<Mutex<()>>,
    /// Prevents pruning restart history before every auto-start query registers
    /// its durable position handle.
    startup_subscriptions_complete: Arc<AtomicBool>,
}

#[async_trait]
impl Source for GitHubWorkGraphSource {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        SOURCE_TYPE
    }
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        use serde_json::Value;
        let mut properties = self.base.properties_or_serialize(&self.config);
        if let Some(webhook) = properties.get_mut("webhook").and_then(Value::as_object_mut) {
            if webhook.get("secret").is_some_and(Value::is_string) {
                webhook.insert("secret".to_string(), serde_json::json!("[REDACTED]"));
            }
            if webhook
                .get("leaseValidationToken")
                .is_some_and(Value::is_string)
            {
                webhook.insert(
                    "leaseValidationToken".to_string(),
                    serde_json::json!("[REDACTED]"),
                );
            }
        }
        if let Some(agent) = properties
            .get_mut("agentConfig")
            .and_then(Value::as_object_mut)
        {
            if agent.get("token").is_some_and(Value::is_string) {
                agent.insert("token".to_string(), serde_json::json!("[REDACTED]"));
            }
        }
        properties
    }
    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }
    fn dispatch_mode(&self) -> DispatchMode {
        self.base.get_dispatch_mode()
    }
    fn supports_replay(&self) -> bool {
        true
    }
    fn describe_schema(&self) -> Option<SourceSchema> {
        Some(SourceSchema {
            nodes: NODE_LABELS.into_iter().map(NodeSchema::new).collect(),
            relations: RELATION_LABELS
                .into_iter()
                .map(RelationSchema::new)
                .collect(),
        })
    }
    async fn start(&self) -> Result<()> {
        match self.base.get_status().await {
            ComponentStatus::Running => return Ok(()),
            ComponentStatus::Error => {
                let id = &self.base.id;
                anyhow::bail!("source '{id}' is in Error; stop it before starting again")
            }
            _ => {}
        }
        report(&self.base, ComponentStatus::Starting, "Starting").await;
        self.base
            .set_position_comparator(ByteLexPositionComparator)
            .await;
        let context = self
            .base
            .context()
            .await
            .ok_or_else(|| anyhow!("initialize() must run before start()"))?;
        let wal = context
            .wal_provider
            .clone()
            .ok_or_else(|| anyhow!("GitHub WorkGraph source requires a WAL provider"))?;
        let state_store = self
            .base
            .state_store()
            .await
            .ok_or_else(|| anyhow!("this source requires a state store provider"))?;
        anyhow::ensure!(
            state_store.is_durable(),
            "this source requires a durable state store (is_durable=true)"
        );
        wal.register(&self.base.id, self.config.durability.to_wal_config())
            .await
            .with_context(|| format!("Failed to register WAL for source '{}'", self.base.id))?;
        let head = wal.head_sequence(&self.base.id).await?;
        // A nonzero head means durable queries may resume from pre-start
        // checkpoints even when all older WAL entries were already pruned.
        self.startup_subscriptions_complete
            .store(head == 0, Ordering::Release);
        self.base.set_next_sequence(head);
        *self.wal.write().await = Some(wal.clone());
        let allocator = Arc::new(Allocator::new(
            self.base.id.clone(),
            state_store,
            wal.clone(),
        ));
        *self.allocator.write().await = Some(allocator.clone());
        allocator
            .recover(chrono::Utc::now().timestamp_millis().max(0) as u64)
            .await?;

        // The agent file is converged once at start-up, before any delivery is
        // accepted, so a Source that missed `push` deliveries while stopped
        // still re-states the configured capacity, and so the retirement ledger
        // reflects what is actually in the graph.
        let agent_sync = match &self.config.agent_config {
            Some(agent_config) => {
                let sync = Arc::new(
                    AgentSync::new(self.base.id.clone(), agent_config, allocator.clone())
                        .context("Failed to build the agent file client")?,
                );
                sync.converge().await.map_err(|error| {
                    anyhow!(
                        "Failed to load the configured agent file '{}' at '{}' ref '{}': {error}",
                        agent_config.path,
                        agent_config.repository,
                        agent_config.r#ref
                    )
                })?;
                *self.agent_sync.write().await = Some(sync.clone());
                Some(sync)
            }
            None => None,
        };

        let (base_tx, base_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        self.base.set_shutdown_tx(base_tx).await;

        let bind_addr = format!("{}:{}", self.config.webhook.host, self.config.webhook.port);
        let listener = TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("Failed to bind webhook listener on {bind_addr}"))?;
        info!(
            "[{}] GitHub WorkGraph webhook listening on {}{}",
            self.base.id, bind_addr, self.config.webhook.path
        );
        let ingress = IngressParams {
            source_id: self.base.id.clone(),
            organization: self.config.organization.clone(),
            repository_filter: self.repository_filter.clone(),
            task_issue_type: self.config.task_issue_type.clone(),
            lease_trust: self.config.lease_trust.clone(),
            path: self.config.webhook.path.clone(),
            secret: self.config.webhook.secret.clone(),
            lease_validation_token: self.config.webhook.lease_validation_token.clone(),
            body_limit_bytes: self.config.webhook.body_limit_bytes,
            allocator: allocator.clone(),
            agent_sync,
            notify: self.notify.clone(),
            shutdown: shutdown_rx.clone(),
        };
        let ingress_base = self.base.clone_shared();
        let ingress_id = self.base.id.clone();
        let mut ingress_task = tokio::spawn(async move {
            if let Err(err) = serve(listener, ingress).await {
                error!("[{ingress_id}] webhook listener failed: {err:#}");
                report(&ingress_base, ComponentStatus::Error, "listener terminated").await;
            }
        });
        let mut dispatch_task = tokio::spawn(dispatch_loop(DispatchLoop {
            base: self.base.clone_shared(),
            wal: wal.clone(),
            notify: self.notify.clone(),
            shutdown: shutdown_rx,
            last_dispatched: head,
            replay_gate: self.replay_gate.clone(),
            allocator,
            startup_subscriptions_complete: self.startup_subscriptions_complete.clone(),
            pruning_fence_started: Instant::now(),
        }));
        let supervisor = tokio::spawn(async move {
            let _ = base_rx.await;
            let _ = shutdown_tx.send(true);
            // `stop_common` aborts only this supervisor, and dropping a child
            // handle detaches instead of cancelling it. Bound the graceful wait
            // and abort both children, so a dispatcher blocked on a full
            // subscriber channel always releases its dispatcher read guard
            // before `stop_common` takes the write guard.
            let drained = async {
                let _ = tokio::join!(&mut ingress_task, &mut dispatch_task);
            };
            if timeout(TASK_GRACE, drained).await.is_err() {
                ingress_task.abort();
                dispatch_task.abort();
                let _ = tokio::join!(ingress_task, dispatch_task);
            }
        });
        self.base.set_task_handle(supervisor).await;
        report(&self.base, ComponentStatus::Running, "Running").await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Stopped {
            return Ok(());
        }
        self.notify.notify_waiters();
        self.base.stop_common().await
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let wal = self
            .wal
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("GitHub WorkGraph source WAL is not initialized"))?;
        let Some(resume_from) = settings.resume_from.clone() else {
            return self
                .base
                .subscribe_with_bootstrap(&settings, SOURCE_TYPE)
                .await;
        };
        let _gate = self.replay_gate.lock().await;
        let resume_seq = decode_position(&resume_from)?;
        let head = wal.head_sequence(&self.base.id).await?;
        let earliest = wal
            .oldest_sequence(&self.base.id)
            .await?
            .unwrap_or(head.saturating_add(1));
        if resume_seq > head || resume_seq.saturating_add(1) < earliest {
            return Err(SourceError::PositionUnavailable {
                source_id: self.base.id.clone(),
                requested: resume_from,
                earliest_available: Some(Bytes::from(earliest.to_be_bytes().to_vec())),
            }
            .into());
        }
        self.base
            .subscribe_with_replay(&settings, wal.as_ref(), resume_seq, SOURCE_TYPE)
            .await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }
    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
    async fn remove_position_handle(&self, query_id: &str) {
        self.base.remove_position_handle(query_id).await;
    }
    async fn on_subscriptions_complete(&self) -> anyhow::Result<()> {
        let allocator = self
            .allocator
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("GitHub WorkGraph allocator is not initialized"))?;
        allocator
            .recover(chrono::Utc::now().timestamp_millis().max(0) as u64)
            .await
            .context("Failed to restate allocator state after startup subscriptions")?;
        self.startup_subscriptions_complete
            .store(true, Ordering::Release);
        self.notify.notify_one();
        Ok(())
    }
    async fn deprovision(&self) -> Result<()> {
        let context = self.base.context().await;
        let wal = self
            .wal
            .read()
            .await
            .clone()
            .or_else(|| context.and_then(|context| context.wal_provider));
        if let Some(wal) = wal {
            wal.delete_wal(&self.base.id)
                .await
                .context("Failed to delete WAL data during deprovision")?;
        }
        if let Some(store) = self.base.state_store().await {
            store
                .clear_store(&self.base.id)
                .await
                .context("Failed to clear delivery dedupe state during deprovision")?;
        }
        self.base.deprovision_common().await
    }
}

struct DispatchLoop {
    base: SourceBase,
    wal: Arc<dyn WalProvider>,
    notify: Arc<Notify>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    last_dispatched: u64,
    replay_gate: Arc<Mutex<()>>,
    allocator: Arc<Allocator>,
    startup_subscriptions_complete: Arc<AtomicBool>,
    pruning_fence_started: Instant,
}

async fn dispatch_loop(mut state: DispatchLoop) {
    let source_id = state.base.get_id().to_string();
    let mut ticker = tokio::time::interval(DRAIN_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_pruned = 0u64;
    loop {
        let tick = tokio::select! {
            biased;
            changed = state.shutdown.changed() => {
                if changed.is_err() || *state.shutdown.borrow() {
                    break;
                }
                false
            }
            _ = state.notify.notified() => { false }
            _ = ticker.tick() => { true }
        };
        if tick {
            let now = chrono::Utc::now();
            if let Err(error) = state
                .allocator
                .expire(now, now.timestamp_millis().max(0) as u64)
                .await
            {
                error!("[{source_id}] allocator expiry failed: {error:#}");
                report(
                    &state.base,
                    ComponentStatus::Error,
                    "allocator state failure",
                )
                .await;
                break;
            }
        }
        let _gate = state.replay_gate.lock().await;
        let head = match state.wal.head_sequence(&source_id).await {
            Ok(head) => head,
            Err(e) => {
                warn!("[{source_id}] failed reading WAL head: {e}");
                continue;
            }
        };
        if head > state.last_dispatched {
            let from = state.last_dispatched + 1;
            match state.wal.read_from(&source_id, from).await {
                Ok(entries) => {
                    let events: Vec<SourceEventWrapper> = entries
                        .into_iter()
                        .filter(|(seq, _)| *seq <= head)
                        .map(|(seq, change)| {
                            state.last_dispatched = state.last_dispatched.max(seq);
                            SourceEventWrapper {
                                source_id: source_id.clone(),
                                event: SourceEvent::Change(change),
                                timestamp: chrono::Utc::now(),
                                sequence: Some(seq),
                                source_position: Some(Bytes::from(seq.to_be_bytes().to_vec())),
                                profiling: None,
                            }
                        })
                        .collect();
                    if !events.is_empty() {
                        if let Err(e) = state.base.dispatch_events_batch(events).await {
                            warn!("[{source_id}] dispatch failed (no subscribers?): {e}");
                        }
                    }
                }
                Err(e) => {
                    warn!("[{source_id}] failed reading WAL from seq {from}: {e}");
                    state.last_dispatched = match state.wal.oldest_sequence(&source_id).await {
                        Ok(Some(oldest)) => state.last_dispatched.max(oldest.saturating_sub(1)),
                        Ok(None) => head,
                        Err(e) => {
                            warn!("[{source_id}] failed reading oldest WAL sequence: {e}");
                            continue;
                        }
                    };
                }
            }
        }
        let subscriptions_complete = state.startup_subscriptions_complete.load(Ordering::Acquire);
        let fallback_elapsed = !subscriptions_complete
            && state.pruning_fence_started.elapsed() >= PRUNING_FENCE_FALLBACK;
        if fallback_elapsed {
            warn!(
                "[{source_id}] startup subscription lifecycle signal was not received; \
                 releasing WAL pruning fence after {}s fallback",
                PRUNING_FENCE_FALLBACK.as_secs()
            );
            state
                .startup_subscriptions_complete
                .store(true, Ordering::Release);
        }
        if subscriptions_complete || fallback_elapsed {
            if let Some(confirmed) = state.base.compute_confirmed_position().await {
                let prune_to = confirmed.min(state.last_dispatched);
                if prune_to > last_pruned {
                    if let Err(e) = state.wal.prune_up_to(&source_id, prune_to).await {
                        warn!("[{source_id}] failed pruning WAL at {prune_to}: {e}");
                    } else {
                        state.base.prune_position_map(prune_to).await;
                        last_pruned = prune_to;
                    }
                }
            }
        }
    }
}

fn decode_position(position: &Bytes) -> Result<u64> {
    let bytes: [u8; 8] = position.as_ref().try_into().map_err(|_| {
        anyhow!(
            "invalid resume_from: expected an 8-byte big-endian u64, got {} byte(s)",
            position.len()
        )
    })?;
    Ok(u64::from_be_bytes(bytes))
}

pub struct GitHubWorkGraphSourceBuilder {
    id: String,
    config: GitHubWorkGraphSourceConfig,
    auto_start: bool,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl GitHubWorkGraphSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: GitHubWorkGraphSourceConfig::default(),
            auto_start: true,
            state_store: None,
        }
    }
    pub fn with_config(mut self, config: GitHubWorkGraphSourceConfig) -> Self {
        self.config = config;
        self
    }
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }
    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
        self
    }
    pub fn build(self) -> Result<GitHubWorkGraphSource> {
        let config = self.config.normalized()?;
        let repository_filter = config.repository_filter()?;
        let mut params = SourceBaseParams::new(&self.id)
            .with_dispatch_mode(DispatchMode::Channel)
            .with_auto_start(self.auto_start);
        if let Some(state_store) = self.state_store {
            params = params.with_state_store(state_store);
        }
        Ok(GitHubWorkGraphSource {
            base: SourceBase::new(params)?,
            config,
            repository_filter,
            wal: Arc::new(RwLock::new(None)),
            allocator: Arc::new(RwLock::new(None)),
            agent_sync: Arc::new(RwLock::new(None)),
            notify: Arc::new(Notify::new()),
            replay_gate: Arc::new(Mutex::new(())),
            startup_subscriptions_complete: Arc::new(AtomicBool::new(true)),
        })
    }
}
