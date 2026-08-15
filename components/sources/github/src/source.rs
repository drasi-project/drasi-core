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

//! Authorized GitHub source implementation.

use crate::bootstrap::GitHubBootstrapProvider;
use crate::config::GitHubSourceConfig;
use crate::graphql::GitHubGraphQLClient;
use crate::hydrator::{
    load_effective_repos, run_hydrator_loop, save_effective_repos, HydratorParams,
};
use crate::mapping::{node_labels, relation_labels};
use crate::reconciler::{run_reconciler_loop, ReconcilerParams};
use crate::types::HydratorHealth;
use crate::webhook::{compact_dedupe_markers, serve_webhook_listener, WebhookServerParams};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, DispatchMode, SubscriptionResponse};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::schema::{NodeSchema, RelationSchema, SourceSchema};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::WalProvider;
use drasi_lib::Source;
use log::{error, info, warn};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration, Instant};

const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// GitHub webhook source with authoritative hydrator/reconciler.
pub struct GitHubSource {
    pub(crate) base: SourceBase,
    config: GitHubSourceConfig,
    wal: Arc<RwLock<Option<Arc<dyn WalProvider>>>>,
    effective_repos: Arc<RwLock<HashSet<String>>>,
    task_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    shutdown_tx: Arc<RwLock<Option<tokio::sync::watch::Sender<bool>>>>,
    hydrator_notify: Arc<Notify>,
    hydrator_health: Arc<RwLock<HydratorHealth>>,
    processing_gate: Arc<Mutex<()>>,
}

#[async_trait]
impl Source for GitHubSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "github"
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn dispatch_mode(&self) -> DispatchMode {
        self.base.get_dispatch_mode()
    }

    fn supports_replay(&self) -> bool {
        false
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        Some(SourceSchema {
            nodes: node_labels().into_iter().map(NodeSchema::new).collect(),
            relations: relation_labels()
                .into_iter()
                .map(RelationSchema::new)
                .collect(),
        })
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting GitHub source".to_string()),
            )
            .await;

        let context = self
            .base
            .context()
            .await
            .ok_or_else(|| anyhow!("Source context not initialized"))?;
        let wal = context
            .wal_provider
            .clone()
            .ok_or_else(|| anyhow!("Durability enabled but no WAL provider configured"))?;
        let state_store = self
            .base
            .state_store()
            .await
            .ok_or_else(|| anyhow!("GitHub source requires a durable state store provider"))?;
        if !state_store.is_durable() {
            return Err(anyhow!(
                "GitHub source requires a durable state store provider (is_durable=true)"
            ));
        }

        if !self.config.durability.enabled {
            return Err(anyhow!(
                "GitHub source requires durability.enabled=true; WAL/state-store durability is mandatory"
            ));
        }

        let wal_config = self.config.durability.to_wal_config();
        wal.register(&self.base.id, wal_config.clone())
            .await
            .map_err(|e| {
                anyhow!(
                    "Failed to register WAL for source '{}': {}",
                    self.base.id,
                    e
                )
            })?;
        *self.wal.write().await = Some(wal.clone());

        let api_client = Arc::new(
            GitHubGraphQLClient::new(self.config.graphql_url.clone(), self.config.token.clone())
                .context("Failed to initialize GitHub API client")?,
        );

        let mut effective = self.config.static_repository_set()?;
        let saved_repos = load_effective_repos(state_store.as_ref(), &self.base.id)
            .await
            .context("Failed to load persisted effective repositories")?;
        if !saved_repos.is_empty() {
            effective = saved_repos;
        }
        for repo in &self.config.repositories {
            effective.insert(repo.to_ascii_lowercase());
        }
        save_effective_repos(state_store.as_ref(), &self.base.id, &effective).await?;
        *self.effective_repos.write().await = effective.clone();
        *self.hydrator_health.write().await = HydratorHealth::default();
        compact_dedupe_markers(state_store.as_ref(), wal.as_ref(), &self.base.id)
            .await
            .context("Failed to compact delivery dedupe markers during startup")?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        // Bind listener before any bootstrap/reconcile work.
        let bind_addr = format!("{}:{}", self.config.webhook.host, self.config.webhook.port);
        let listener = TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("Failed to bind webhook listener on {bind_addr}"))?;
        info!("[{}] Webhook listener bound on {}", self.base.id, bind_addr);

        let webhook_params = WebhookServerParams {
            source_id: self.base.id.clone(),
            host: self.config.webhook.host.clone(),
            port: self.config.webhook.port,
            path: self.config.webhook.path.clone(),
            body_limit_bytes: self.config.webhook.body_limit_bytes,
            secret: self.config.webhook.secret.clone(),
            wal: wal.clone(),
            state_store: state_store.clone(),
            hydrator_notify: self.hydrator_notify.clone(),
            hydrator_health: self.hydrator_health.clone(),
            shutdown: shutdown_rx.clone(),
        };
        let webhook_handle = tokio::spawn(async move {
            if let Err(err) = serve_webhook_listener(listener, webhook_params).await {
                error!("GitHub webhook listener failed: {err:#}");
            }
        });

        let hydrator_params = HydratorParams {
            source_id: self.base.id.clone(),
            base: self.base.clone_shared(),
            wal: wal.clone(),
            state_store: state_store.clone(),
            api_client: api_client.clone(),
            projects: self.config.projects.clone(),
            effective_repos: self.effective_repos.clone(),
            notify: self.hydrator_notify.clone(),
            health: self.hydrator_health.clone(),
            processing_gate: self.processing_gate.clone(),
            shutdown: shutdown_rx.clone(),
        };
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("GitHub source running".to_string()),
            )
            .await;
        let hydrator_failure_health = self.hydrator_health.clone();
        let hydrator_failure_base = self.base.clone_shared();
        let hydrator_failure_source_id = self.base.id.clone();
        let hydrator_handle = tokio::spawn(async move {
            if let Err(err) = run_hydrator_loop(hydrator_params).await {
                let message = format!("{err:#}");
                {
                    let mut health = hydrator_failure_health.write().await;
                    health.terminal = true;
                    health.next_retry_secs = None;
                    health.last_error = Some(message.clone());
                }
                hydrator_failure_base
                    .set_status(
                        ComponentStatus::Error,
                        Some("GitHub hydrator terminated; source restart required".to_string()),
                    )
                    .await;
                error!(
                    "[{hydrator_failure_source_id}] GitHub hydrator task failed terminally: {message}"
                );
            }
        });

        let reconciler_params = ReconcilerParams {
            source_id: self.base.id.clone(),
            base: self.base.clone_shared(),
            state_store,
            api_client,
            projects: self.config.projects.clone(),
            static_repos: self.config.static_repository_set().unwrap_or_default(),
            effective_repos: self.effective_repos.clone(),
            interval_secs: self.config.reconcile_interval_secs,
            run_initial_pass: !self.config.skip_initial_bootstrap,
            processing_gate: self.processing_gate.clone(),
            shutdown: shutdown_rx,
        };
        let reconciler_handle = tokio::spawn(async move {
            if let Err(err) = run_reconciler_loop(reconciler_params).await {
                error!("GitHub reconciler task failed: {err:#}");
            }
        });

        {
            let mut handles = self.task_handles.write().await;
            handles.push(webhook_handle);
            handles.push(hydrator_handle);
            handles.push(reconciler_handle);
        }

        info!("[{}] GitHub source started", self.base.id);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Stopped {
            return Ok(());
        }
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping GitHub source".to_string()),
            )
            .await;

        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(true);
        }

        let mut handles = {
            let mut task_handles = self.task_handles.write().await;
            std::mem::take(&mut *task_handles)
        };
        let shutdown_deadline = Instant::now() + TASK_SHUTDOWN_TIMEOUT;
        while let Some(mut handle) = handles.pop() {
            let remaining = shutdown_deadline.saturating_duration_since(Instant::now());
            match timeout(remaining, &mut handle).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    warn!("[{}] Source task panicked: {}", self.base.id, err);
                }
                Err(_) => {
                    warn!(
                        "[{}] Timed out waiting for task shutdown; aborting it",
                        self.base.id
                    );
                    handle.abort();
                    if let Err(err) = handle.await {
                        if !err.is_cancelled() {
                            warn!("[{}] Aborted source task failed: {}", self.base.id, err);
                        }
                    }
                }
            }
        }

        self.base.clear_dispatchers().await;
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("GitHub source stopped".to_string()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "github")
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

    async fn deprovision(&self) -> Result<()> {
        if let Some(wal) = self.wal.read().await.clone() {
            wal.delete_wal(&self.base.id)
                .await
                .context("Failed to delete source WAL data during deprovision")?;
        }
        self.base.deprovision_common().await
    }
}

/// Builder for [`GitHubSource`].
pub struct GitHubSourceBuilder {
    id: String,
    config: GitHubSourceConfig,
    auto_start: bool,
    dispatch_mode: DispatchMode,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl GitHubSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: GitHubSourceConfig::default(),
            auto_start: true,
            dispatch_mode: DispatchMode::Channel,
            bootstrap_provider: None,
            state_store: None,
        }
    }

    pub fn with_config(mut self, config: GitHubSourceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = mode;
        self
    }

    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
        self
    }

    pub fn build(self) -> Result<GitHubSource> {
        self.config.validate()?;

        let mut params = SourceBaseParams::new(&self.id)
            .with_dispatch_mode(self.dispatch_mode)
            .with_auto_start(self.auto_start);

        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        } else {
            params =
                params.with_bootstrap_provider(GitHubBootstrapProvider::new(self.config.clone()));
        }

        if let Some(state_store) = self.state_store {
            params = params.with_state_store(state_store);
        }

        let base = SourceBase::new(params)?;
        Ok(GitHubSource {
            base,
            config: self.config,
            wal: Arc::new(RwLock::new(None)),
            effective_repos: Arc::new(RwLock::new(HashSet::new())),
            task_handles: Arc::new(RwLock::new(Vec::new())),
            shutdown_tx: Arc::new(RwLock::new(None)),
            hydrator_notify: Arc::new(Notify::new()),
            hydrator_health: Arc::new(RwLock::new(HydratorHealth::default())),
            processing_gate: Arc::new(Mutex::new(())),
        })
    }
}
