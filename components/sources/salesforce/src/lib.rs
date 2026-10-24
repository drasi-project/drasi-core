#![allow(unexpected_cfgs)]

mod bootstrap;
mod config;
mod replay;
mod runtime;
mod store;

pub mod descriptor;

pub use config::SalesforceSourceConfig;
pub use store::SalesforceSchema;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::channels::SubscriptionResponse;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::identity::IdentityProvider;
use drasi_lib::schema::SourceSchema;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::{ByteLexPositionComparator, Source, SourceError};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::ComponentStatus;
use drasi_salesforce_common::auth::SalesforceAuthManager;
use drasi_salesforce_common::rest::SalesforceRestClient;
use drasi_salesforce_common::{
    LocalPosition, SnapshotTicket, SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY,
};
use futures_util::FutureExt;
use log::{error, info, warn};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::watch;
use tokio::sync::{Mutex, RwLock};
use tracing::Instrument;

use crate::replay::JournalReplayReceiver;
use crate::runtime::{discover_catalog, RuntimeContext};
use crate::store::{DurableMirrorStore, StorageLimits};

pub struct SalesforceSource {
    base: SourceBase,
    config: SalesforceSourceConfig,
    configured_identity_provider: Option<Arc<dyn IdentityProvider>>,
    store: Arc<RwLock<Option<Arc<DurableMirrorStore>>>>,
    store_initializer: Mutex<Option<tokio::task::JoinHandle<Result<()>>>>,
    lifecycle_lock: Mutex<()>,
    active_queries: Arc<RwLock<HashMap<String, Weak<()>>>>,
    delivery_gate: Arc<Mutex<()>>,
    shutdown_tx: Arc<RwLock<Option<watch::Sender<bool>>>>,
    schema_snapshot: watch::Sender<Option<Arc<SourceSchema>>>,
}

#[cfg(test)]
const STOP_TASK_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const STOP_TASK_TIMEOUT: Duration = Duration::from_secs(10);

fn spawn_runtime_task<Fut>(
    source_id: String,
    base: SourceBase,
    runtime_future: Fut,
    span: tracing::Span,
) -> tokio::task::JoinHandle<()>
where
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(
        async move {
            let outcome = match std::panic::AssertUnwindSafe(runtime_future)
                .catch_unwind()
                .await
            {
                Ok(result) => result,
                Err(_) => Err(anyhow!("Salesforce runtime panicked")),
            };
            match outcome {
                Ok(()) => {
                    if matches!(
                        base.get_status().await,
                        ComponentStatus::Running | ComponentStatus::Starting
                    ) {
                        let message =
                            "Salesforce source runtime exited unexpectedly without shutdown request"
                                .to_string();
                        error!("[{source_id}] {message}");
                        base.set_status(ComponentStatus::Error, Some(message)).await;
                    }
                }
                Err(error) => {
                    let message = format!("Salesforce source runtime failed: {error:#}");
                    error!("[{source_id}] {message}");
                    base.set_status(ComponentStatus::Error, Some(message)).await;
                }
            }
        }
        .instrument(span),
    )
}

impl SalesforceSource {
    pub fn builder(id: impl Into<String>) -> SalesforceSourceBuilder {
        SalesforceSourceBuilder::new(id)
    }

    pub async fn schema(&self) -> Result<SalesforceSchema> {
        let store = self
            .store
            .read()
            .await
            .as_ref()
            .cloned()
            .context("Salesforce schema is not ready; start the source first")?;
        store.schema().await
    }

    async fn ensure_store(&self) -> Result<Arc<DurableMirrorStore>> {
        if let Some(existing) = self.store.read().await.as_ref() {
            return Ok(existing.clone());
        }

        let limits = StorageLimits {
            max_rows: self.config.max_rows,
            max_events: self.config.max_events,
            retention_hours: self.config.retention_hours,
            max_projected_row_bytes: self.config.max_projected_row_bytes,
            max_snapshot_leases: self.config.max_snapshot_leases,
            max_spool_bytes: self.config.max_spool_bytes,
        };

        let config = self.config.clone();
        let source_id = self.base.id.clone();
        self.get_or_initialize_store(async move {
            Ok(Arc::new(
                DurableMirrorStore::open(
                    std::path::PathBuf::from(&config.spool_root),
                    source_id,
                    config.source_identity(),
                    config.connection.expected_org_id,
                    config.objects,
                    limits,
                )
                .await
                .context("failed to open Salesforce durable mirror store")?,
            ))
        })
        .await
    }

    async fn get_or_initialize_store(
        &self,
        create: impl Future<Output = Result<Arc<DurableMirrorStore>>> + Send + 'static,
    ) -> Result<Arc<DurableMirrorStore>> {
        let mut initializer = self.store_initializer.lock().await;
        if let Some(existing) = self.store.read().await.as_ref() {
            return Ok(existing.clone());
        }
        if initializer.is_none() {
            let base = self.base.clone_shared();
            let published = self.store.clone();
            *initializer = Some(tokio::spawn(async move {
                let store = create.await?;
                base.set_next_sequence(store.head_ordinal().await?);
                *published.write().await = Some(store);
                Ok(())
            }));
        }
        let result = initializer
            .as_mut()
            .context("missing store initializer")?
            .await;
        *initializer = None;
        result.context("Salesforce store initializer task failed")??;
        self.store
            .read()
            .await
            .as_ref()
            .cloned()
            .context("store initialization did not publish a store")
    }

    async fn finish_store_initialization(&self) -> Result<()> {
        let mut initializer = self.store_initializer.lock().await;
        if let Some(task) = initializer.as_mut() {
            let result = task.await;
            *initializer = None;
            result.context("Salesforce store initialization task failed")??;
        }
        Ok(())
    }

    async fn resolve_identity_provider(&self) -> Option<Arc<dyn IdentityProvider>> {
        if let Some(injected) = self.base.identity_provider().await {
            return Some(injected);
        }
        self.configured_identity_provider.clone()
    }

    async fn validate_resume_position(
        &self,
        store: &DurableMirrorStore,
        requested: &Bytes,
    ) -> Result<u64> {
        let decoded =
            LocalPosition::decode(requested).map_err(|_| SourceError::PositionUnavailable {
                source_id: self.base.id.clone(),
                requested: requested.clone(),
                earliest_available: None,
            })?;

        let head = store.head_ordinal().await?;
        let earliest_resumable = store.earliest_resumable_ordinal().await?;
        let earliest_position = LocalPosition::new(store.epoch(), earliest_resumable).encode();
        if decoded.epoch != store.epoch()
            || decoded.ordinal < earliest_resumable
            || decoded.ordinal > head
        {
            return Err(SourceError::PositionUnavailable {
                source_id: self.base.id.clone(),
                requested: requested.clone(),
                earliest_available: Some(earliest_position),
            }
            .into());
        }

        Ok(decoded.ordinal)
    }

    fn validate_resume_sequence_consistency(
        &self,
        query_id: &str,
        resume_ordinal: u64,
        resume_sequence: u64,
    ) -> Result<()> {
        if resume_sequence != 0 && resume_sequence != resume_ordinal {
            return Err(anyhow!(
                "query '{}' provided inconsistent resume checkpoint for source '{}': resume_sequence={} must be the bootstrap sentinel 0 or equal resume_from cursor ordinal={}",
                query_id,
                self.base.id,
                resume_sequence,
                resume_ordinal
            ));
        }
        Ok(())
    }

    async fn reap_finished_runtime_task_handle(&self) -> Result<()> {
        let finished = {
            let mut guard = self.base.task_handle.write().await;
            if guard.as_ref().is_some_and(|handle| handle.is_finished()) {
                guard.take()
            } else {
                None
            }
        };

        if let Some(handle) = finished {
            match handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!(
                                "Salesforce source runtime task panicked while reaping stale handle: {error}"
                            )),
                        )
                        .await;
                    return Err(anyhow::Error::new(error)
                        .context("failed to reap stale Salesforce runtime task handle"));
                }
            }
        }

        Ok(())
    }

    async fn runtime_task_alive(&self) -> bool {
        self.base
            .task_handle
            .read()
            .await
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }
}

#[async_trait]
impl Source for SalesforceSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "salesforce"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::SalesforceSourceConfigDto;
        self.base
            .properties_or_serialize(&SalesforceSourceConfigDto::from(&self.config))
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        let snapshot = self.schema_snapshot.borrow().clone();
        snapshot.map(|schema| (*schema).clone())
    }

    async fn start(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        self.reap_finished_runtime_task_handle().await?;

        if self.base.get_status().await == ComponentStatus::Running {
            if self.runtime_task_alive().await {
                return Ok(());
            }
            let message =
                "Salesforce source was marked Running but runtime task is no longer active"
                    .to_string();
            self.base
                .set_status(ComponentStatus::Error, Some(message.clone()))
                .await;
            return Err(anyhow!(message));
        }

        if self.runtime_task_alive().await {
            return Err(anyhow!(
                "cannot start Salesforce source '{}': prior runtime task is still active",
                self.base.id
            ));
        }

        if self
            .store_initializer
            .try_lock()
            .map_or(true, |task| task.is_some())
        {
            return Err(anyhow!(
                "cannot start Salesforce source while prior store initialization remains attached; call stop to finish it"
            ));
        }
        if let Some(store) = self.store.read().await.as_ref() {
            if store.has_snapshot_export().await {
                return Err(anyhow!("cannot start Salesforce source while a prior snapshot export remains attached; call stop to finish it"));
            }
        }

        if matches!(
            self.base.get_status().await,
            ComponentStatus::Starting | ComponentStatus::Stopping
        ) {
            return Err(anyhow!(
                "cannot start Salesforce source '{}': lifecycle transition already in progress",
                self.base.id
            ));
        }

        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting Salesforce source".to_string()),
            )
            .await;

        let start_result: Result<()> = async {
            let store = self.ensure_store().await?;
            let provider = self.resolve_identity_provider().await;
            let auth = SalesforceAuthManager::new(self.config.connection.clone(), provider)?;
            let rest = SalesforceRestClient::new(self.config.connection.clone())?;

            let source_id = self.base.id.clone();
            let base = self.base.clone_shared();
            let config = self.config.clone();
            let delivery_gate = self.delivery_gate.clone();
            let schema_snapshot = self.schema_snapshot.clone();
            let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
            *self.shutdown_tx.write().await = Some(shutdown_tx.clone());

            let instance_id = self
                .base
                .context()
                .await
                .map(|ctx| ctx.instance_id)
                .unwrap_or_default();
            let span = tracing::info_span!(
                "salesforce_source_runtime",
                instance_id = %instance_id,
                component_id = %source_id,
                component_type = "source"
            );

            let (start_tx, start_rx) = tokio::sync::oneshot::channel();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let runtime_base = base.clone_shared();
            let runtime_source_id = source_id.clone();
            let runtime_future = async move {
                let prepared = async {
                    let catalog = tokio::select! {
                        _ = shutdown_rx.changed() => return Err(anyhow!("Salesforce startup was stopped")),
                        result = discover_catalog(&config, &auth, &rest) => result?,
                    };
                    let schema = Arc::new(catalog.source_schema());
                    let _gate = delivery_gate.lock().await;
                    store.set_schema(catalog.schema.clone()).await?;
                    store.publish_schema().await?;
                    schema_snapshot.send_replace(Some(schema));
                    Ok::<_, anyhow::Error>(Arc::new(catalog))
                }.await;
                let catalog = match prepared {
                    Ok(catalog) => catalog,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("{error:#}")));
                        return Err(error);
                    }
                };
                ready_tx.send(Ok(())).map_err(|_| anyhow!("Salesforce startup caller was cancelled"))?;
                start_rx.await.context("Salesforce startup was cancelled")?;
                RuntimeContext {
                    source_id: runtime_source_id,
                    config,
                    catalog,
                    auth,
                    rest,
                    store,
                    base,
                    delivery_gate,
                    shutdown: shutdown_tx,
                    schema_snapshot,
                }.run(shutdown_rx).await
            };
            let mut task_handle = self.base.task_handle.write().await;
            *task_handle = Some(spawn_runtime_task(
                source_id,
                runtime_base,
                runtime_future,
                span,
            ));
            drop(task_handle);
            ready_rx.await.context("Salesforce discovery task stopped before readiness")?
                .map_err(|message| anyhow!(message))?;
            self.base
                .set_status(
                    ComponentStatus::Running,
                    Some("Salesforce source running".into()),
                )
                .await;
            start_tx
                .send(())
                .map_err(|_| anyhow!("Salesforce runtime ended before startup completed"))
        }
        .await;

        if let Err(error) = start_result {
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Salesforce source failed to start: {error:#}")),
                )
                .await;
            return Err(error);
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping Salesforce source".to_string()),
            )
            .await;

        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(true);
        }

        let mut task_handle = self.base.task_handle.write().await;
        if let Some(handle) = task_handle.as_mut() {
            enum JoinOutcome {
                Completed(std::result::Result<(), tokio::task::JoinError>),
                TimedOut,
            }

            let join_outcome = tokio::select! {
                result = handle => JoinOutcome::Completed(result),
                _ = tokio::time::sleep(STOP_TASK_TIMEOUT) => JoinOutcome::TimedOut,
            };

            match join_outcome {
                JoinOutcome::Completed(Ok(())) => {}
                JoinOutcome::Completed(Err(error)) if error.is_cancelled() => {}
                JoinOutcome::Completed(Err(error)) => {
                    *task_handle = None;
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!(
                                "Salesforce source task panicked during stop: {error}"
                            )),
                        )
                        .await;
                    return Err(anyhow::Error::new(error)
                        .context("Salesforce source task panicked during stop"));
                }
                JoinOutcome::TimedOut => {
                    warn!(
                        "Timed out waiting for Salesforce source task to stop; refusing to detach running task"
                    );
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some("Salesforce source stop timeout left runtime active; source refused to detach running task".to_string()),
                        )
                        .await;
                    return Err(anyhow!(
                        "Salesforce source runtime did not stop within timeout and remains attached"
                    ));
                }
            }
            *task_handle = None;
        }
        drop(task_handle);

        if let Err(error) =
            tokio::time::timeout(STOP_TASK_TIMEOUT, self.finish_store_initialization())
                .await
                .context(
                    "Salesforce store initialization stop timeout; initializer remains attached",
                )
                .and_then(|result| result)
        {
            self.base
                .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                .await;
            return Err(error);
        }

        if let Some(store) = self.store.read().await.as_ref() {
            if let Err(error) =
                tokio::time::timeout(STOP_TASK_TIMEOUT, store.finish_snapshot_export())
                    .await
                    .context("Salesforce snapshot export stop timeout; export remains attached")
                    .and_then(|result| result)
            {
                self.base
                    .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                    .await;
                return Err(error);
            }
        }
        let _delivery = self.delivery_gate.lock().await;
        self.base.clear_dispatchers().await;
        self.active_queries.write().await.clear();
        if let Some(store) = self.store.read().await.as_ref() {
            store.clear_retention_pins()?;
        }

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Salesforce source stopped".to_string()),
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
        let _gate = self.delivery_gate.lock().await;
        if settings.resume_from.is_none()
            && settings
                .resume_sequence
                .is_some_and(|sequence| sequence != 0)
        {
            return Err(anyhow!(
                "Salesforce recovery requires a local position with a nonzero resume sequence"
            ));
        }
        let store = self.ensure_store().await?;
        {
            let mut active = self.active_queries.write().await;
            active.retain(|_, subscription| subscription.strong_count() > 0);
            if !active.contains_key(&settings.query_id)
                && active.len() >= self.config.max_subscribers
            {
                return Err(anyhow!(
                    "source '{}' reached max_subscribers limit ({})",
                    self.base.id,
                    self.config.max_subscribers
                ));
            }
        }

        let mut bootstrap_properties = HashMap::new();
        let mut replay_start_exclusive = 0u64;

        if let Some(ref requested) = settings.resume_from {
            replay_start_exclusive = self
                .validate_resume_position(&store, requested)
                .await
                .with_context(|| {
                    format!(
                        "query '{}' cannot resume on Salesforce source '{}'",
                        settings.query_id, self.base.id
                    )
                })?;

            if let Some(resume_sequence) = settings.resume_sequence {
                self.validate_resume_sequence_consistency(
                    &settings.query_id,
                    replay_start_exclusive,
                    resume_sequence,
                )?;
            }
        }

        let (mut response, subscription_lifetime) = {
            if settings.resume_from.is_none() && settings.enable_bootstrap {
                let ticket = store
                    .create_snapshot_ticket(self.config.snapshot_ticket_ttl_seconds)
                    .await
                    .context("failed to create bootstrap snapshot ticket")?;
                replay_start_exclusive = ticket.head_ordinal;
                bootstrap_properties.insert(
                    SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
                    serde_json::to_value(ticket)?,
                );
            }

            let mut response = self
                .base
                .subscribe_with_bootstrap_context(&settings, "Salesforce", bootstrap_properties)
                .await?;

            let cut_ordinal = store.head_ordinal().await?;
            if settings.resume_from.is_none() && !settings.enable_bootstrap {
                replay_start_exclusive = cut_ordinal;
            }
            let replay_pin = match &response.position_handle {
                Some(handle) => {
                    Some(store.register_subscription_pin(replay_start_exclusive, handle)?)
                }
                None => {
                    store
                        .register_replay_pin(replay_start_exclusive, cut_ordinal)
                        .await?
                }
            };

            let receiver = JournalReplayReceiver::new(
                store.clone(),
                replay_start_exclusive,
                cut_ordinal,
                self.config.max_replay_page_events,
                self.config.max_replay_page_bytes,
                replay_pin,
                response.receiver,
            );
            let subscription_lifetime = receiver.subscription_lifetime();
            response.receiver = Box::new(receiver);
            (response, subscription_lifetime)
        };

        response.source_id = self.base.id.clone();
        self.active_queries
            .write()
            .await
            .insert(settings.query_id.clone(), subscription_lifetime);
        Ok(response)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn deprovision(&self) -> Result<()> {
        self.stop()
            .await
            .context("failed to stop Salesforce source before deprovision")?;
        let _lifecycle = self.lifecycle_lock.lock().await;
        if self.runtime_task_alive().await {
            return Err(anyhow!("Salesforce source restarted before deprovision"));
        }
        let _delivery = self.delivery_gate.lock().await;
        let store = self.ensure_store().await?;
        self.finish_store_initialization().await?;
        store.deprovision().await?;
        *self.store.write().await = None;
        self.schema_snapshot.send_replace(None);
        self.base.deprovision_common().await
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
        self.base
            .set_position_comparator(ByteLexPositionComparator)
            .await;
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        self.base.set_bootstrap_provider(provider).await;
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.active_queries.write().await.remove(query_id);
        self.base.remove_position_handle(query_id).await;
    }

    async fn set_identity_provider(&self, provider: Arc<dyn IdentityProvider>) {
        self.base.set_identity_provider(provider).await;
    }
}

pub struct SalesforceSourceBuilder {
    id: String,
    config: Option<SalesforceSourceConfig>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    identity_provider: Option<Arc<dyn IdentityProvider>>,
    dispatch_buffer_capacity: Option<usize>,
    auto_start: bool,
}

impl SalesforceSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: None,
            bootstrap_provider: None,
            state_store: None,
            identity_provider: None,
            dispatch_buffer_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_config(mut self, config: SalesforceSourceConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_bootstrap_provider(mut self, provider: impl BootstrapProvider + 'static) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
        self
    }

    pub fn with_identity_provider(mut self, provider: Arc<dyn IdentityProvider>) -> Self {
        self.identity_provider = Some(provider);
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn build(self) -> Result<SalesforceSource> {
        let config = self
            .config
            .ok_or_else(|| anyhow!("Salesforce source configuration is required"))?;
        config.validate()?;

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        let bootstrap = match self.bootstrap_provider {
            Some(provider) => provider,
            None => Box::new(
                bootstrap::SalesforceBootstrapProvider::builder()
                    .with_spool_root(&config.spool_root)
                    .with_max_rows(config.max_rows)
                    .with_max_row_bytes(config.max_projected_row_bytes)
                    .build()?,
            ),
        };
        params = params.with_bootstrap_provider(bootstrap);
        if let Some(store) = self.state_store {
            params = params.with_state_store(store);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }

        Ok(SalesforceSource {
            base: SourceBase::new(params)?,
            config,
            configured_identity_provider: self.identity_provider,
            store: Arc::new(RwLock::new(None)),
            store_initializer: Mutex::new(None),
            lifecycle_lock: Mutex::new(()),
            active_queries: Arc::new(RwLock::new(HashMap::new())),
            delivery_gate: Arc::new(Mutex::new(())),
            shutdown_tx: Arc::new(RwLock::new(None)),
            schema_snapshot: watch::channel(None).0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::SalesforceBootstrapProvider;
    use anyhow::Context;
    use async_trait::async_trait;
    use drasi_core::models::{
        Element, ElementMetadata, ElementReference, ElementValue, SourceChange,
    };
    use drasi_lib::bootstrap::{
        BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
    };
    use drasi_lib::channels::{
        BootstrapEvent, BootstrapEventSender, SourceControl, SourceEvent, SourceEventWrapper,
    };
    use drasi_lib::state_store::{MemoryStateStoreProvider, StateStoreError, StateStoreResult};
    use drasi_salesforce_common::{
        GraphRecord, LocalPosition, ObjectPlan, Properties, SnapshotTicket,
        SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY,
    };
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;
    use tempfile::TempDir;
    use tokio::time::{sleep, timeout, Duration};

    use crate::store::RecordMutation;

    #[derive(Default)]
    struct RejectClearStateStore(MemoryStateStoreProvider);

    #[async_trait]
    impl StateStoreProvider for RejectClearStateStore {
        async fn get(&self, store: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
            self.0.get(store, key).await
        }
        async fn set(&self, store: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
            self.0.set(store, key, value).await
        }
        async fn delete(&self, store: &str, key: &str) -> StateStoreResult<bool> {
            self.0.delete(store, key).await
        }
        async fn contains_key(&self, store: &str, key: &str) -> StateStoreResult<bool> {
            self.0.contains_key(store, key).await
        }
        async fn get_many(
            &self,
            store: &str,
            keys: &[&str],
        ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
            self.0.get_many(store, keys).await
        }
        async fn set_many(&self, store: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
            self.0.set_many(store, entries).await
        }
        async fn delete_many(&self, store: &str, keys: &[&str]) -> StateStoreResult<usize> {
            self.0.delete_many(store, keys).await
        }
        async fn clear_store(&self, _store: &str) -> StateStoreResult<usize> {
            Err(StateStoreError::StorageError("clear rejected".into()))
        }
        async fn list_keys(&self, store: &str) -> StateStoreResult<Vec<String>> {
            self.0.list_keys(store).await
        }
        async fn store_exists(&self, store: &str) -> StateStoreResult<bool> {
            self.0.store_exists(store).await
        }
        async fn key_count(&self, store: &str) -> StateStoreResult<usize> {
            self.0.key_count(store).await
        }
    }

    #[derive(Clone)]
    struct SequencedBootstrapProvider {
        row_count: usize,
    }

    #[async_trait]
    impl BootstrapProvider for SequencedBootstrapProvider {
        async fn bootstrap(
            &self,
            _request: BootstrapRequest,
            context: &BootstrapContext,
            event_tx: BootstrapEventSender,
            _settings: Option<&SourceSubscriptionSettings>,
        ) -> Result<BootstrapResult> {
            let ticket = context
                .get_typed_property::<SnapshotTicket>(SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY)?
                .context("missing snapshot ticket")?;

            for i in 0..self.row_count {
                let mut properties = drasi_core::models::ElementPropertyMap::new();
                properties.insert(
                    "Id",
                    ElementValue::String(format!("bootstrap-{i:03}").into()),
                );
                properties.insert("Name", ElementValue::String("bootstrap".into()));

                event_tx
                    .send(BootstrapEvent {
                        source_id: context.source_id.clone(),
                        change: SourceChange::Insert {
                            element: Element::Node {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new(
                                        &context.source_id,
                                        &format!("sf:00D000000000001AAA:Account:bootstrap-{i:03}"),
                                    ),
                                    labels: vec![Arc::from("Account")].into(),
                                    effective_from: i as u64,
                                },
                                properties,
                            },
                        },
                        timestamp: chrono::Utc::now(),
                        sequence: context.next_sequence(),
                    })
                    .await
                    .context("failed to emit bootstrap row")?;
            }

            Ok(BootstrapResult {
                event_count: self.row_count,
                source_position: Some(ticket.local_position()),
            })
        }
    }

    fn test_config(spool_root: &str) -> SalesforceSourceConfig {
        SalesforceSourceConfig {
            connection: drasi_salesforce_common::SalesforceConnectionConfig {
                instance_url: "http://127.0.0.1:19080".to_string(),
                expected_org_id: "00D000000000001AAA".to_string(),
                api_version: "68.0".to_string(),
                auth: drasi_salesforce_common::SalesforceAuthConfig::ClientCredentials {
                    token_url: "http://127.0.0.1:19080/oauth/token".to_string(),
                    client_id: "client-id".to_string(),
                    client_secret: "client-secret".to_string(),
                },
            },
            objects: vec!["Account".to_string()],
            pubsub_endpoint: "http://127.0.0.1:19081".to_string(),
            reconcile_interval_seconds: 1,
            full_scan_interval_seconds: 1,
            max_rows: 100,
            max_events: 100,
            retention_hours: 1,
            max_projected_row_bytes: 1024 * 1024,
            max_scan_bytes: 1024 * 1024,
            max_replay_page_events: 10,
            max_replay_page_bytes: 64 * 1024,
            max_subscribers: 16,
            spool_root: spool_root.to_string(),
            snapshot_ticket_ttl_seconds: 60,
            max_snapshot_leases: 4,
            max_spool_bytes: 8 * 1024 * 1024 * 1024,
            initial_cursor_behavior: drasi_salesforce_common::InitialCursorBehavior::Latest,
        }
    }

    fn settings(query_id: &str) -> SourceSubscriptionSettings {
        SourceSubscriptionSettings {
            source_id: "salesforce-test-source".to_string(),
            enable_bootstrap: false,
            query_id: query_id.to_string(),
            nodes: HashSet::new(),
            relations: HashSet::new(),
            resume_from: None,
            resume_sequence: None,
            request_position_handle: false,
        }
    }

    fn control_event(source_id: &str, sequence: u64) -> SourceEventWrapper {
        SourceEventWrapper::new(
            source_id.to_string(),
            SourceEvent::Control(SourceControl::FuturesDue),
            chrono::Utc::now(),
            sequence,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schema_snapshot_is_owned_and_independent_of_async_store_access() -> Result<()> {
        let temp = TempDir::new()?;
        let source = SalesforceSource::builder("schema-source")
            .with_config(test_config(temp.path().join("spool").to_str().unwrap()))
            .build()?;
        assert_eq!(source.describe_schema(), None);
        let expected = SourceSchema {
            nodes: vec![drasi_lib::schema::NodeSchema::new("Example")],
            relations: Vec::new(),
        };
        assert_eq!(source.schema_snapshot.receiver_count(), 0);
        source
            .schema_snapshot
            .send_replace(Some(Arc::new(expected.clone())));
        let _store = source.store.write().await;
        let mut owned = source.describe_schema().context("published schema")?;
        owned.nodes.clear();
        assert_eq!(source.describe_schema(), Some(expected));
        assert!(!temp.path().join("spool").exists());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schema_stays_cleared_when_common_deprovision_fails() -> Result<()> {
        let temp = TempDir::new()?;
        let spool = temp.path().join("spool");
        let source = SalesforceSource::builder("schema-source")
            .with_config(test_config(spool.to_str().unwrap()))
            .with_state_store(Arc::new(RejectClearStateStore::default()))
            .build()?;
        source.ensure_store().await?;
        source
            .schema_snapshot
            .send_replace(Some(Arc::new(SourceSchema {
                nodes: vec![drasi_lib::schema::NodeSchema::new("Example")],
                relations: Vec::new(),
            })));
        let error = source.deprovision().await.unwrap_err();
        assert!(format!("{error:#}").contains("clear rejected"));
        assert!(!spool.join("mirror.redb").exists());
        assert!(source.store.read().await.is_none());
        assert_eq!(source.describe_schema(), None);
        Ok(())
    }

    fn insert_mutation(
        _source_id: &str,
        _org_id: &str,
        plan: &ObjectPlan,
        id: &str,
        name: &str,
        observed_at_ms: u64,
    ) -> Result<RecordMutation> {
        let mut properties = Properties::new();
        properties.insert("Name".to_string(), ElementValue::String(name.into()));

        let row = GraphRecord {
            object: plan.name.clone(),
            id: id.to_string(),
            properties,
            observed_at_ms,
            relationships: Vec::new(),
            schema_revision: String::new(),
        };
        Ok(RecordMutation::Upsert(row))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_resume_does_not_consume_max_subscribers_quota() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let mut config = test_config(spool_root.to_string_lossy().as_ref());
        config.max_subscribers = 1;
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(config)
            .build()?;

        let bad_position = Bytes::from_static(&[0xFF, 0x00, 0x01]);
        let mut bad_settings = settings("bad-position");
        bad_settings.resume_from = Some(bad_position.clone());

        let error = match source.subscribe(bad_settings).await {
            Ok(_) => anyhow::bail!("invalid cursor must fail with PositionUnavailable"),
            Err(error) => error,
        };
        let Some(SourceError::PositionUnavailable {
            source_id,
            requested,
            ..
        }) = error.downcast_ref::<SourceError>()
        else {
            panic!("expected SourceError::PositionUnavailable, got: {error:#}");
        };
        assert_eq!(source_id, "salesforce-test-source");
        assert_eq!(requested, &bad_position);

        let good = source.subscribe(settings("snapshot")).await?;
        assert_eq!(good.query_id, "snapshot");
        let active = source.active_queries.read().await.clone();
        assert!(active.contains_key("snapshot"));
        assert!(!active.contains_key("bad-position"));
        assert_eq!(
            active.len(),
            1,
            "failed resume must not consume max_subscribers quota"
        );

        drop(good);
        source.remove_position_handle("snapshot").await;

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn late_subscriber_waits_for_delivery_gate_under_backpressure() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = Arc::new(
            SalesforceSource::builder("salesforce-test-source")
                .with_config(test_config(spool_root.to_string_lossy().as_ref()))
                .with_dispatch_buffer_capacity(1)
                .build()?,
        );

        let mut primary = source.subscribe(settings("q1")).await?;
        let first = control_event(source.id(), source.base.next_sequence());
        let second = control_event(source.id(), source.base.next_sequence());

        let dispatch_source = source.clone();
        let dispatch_task = tokio::spawn(async move {
            let _gate = dispatch_source.delivery_gate.lock().await;
            dispatch_source
                .base
                .dispatch_events_batch(vec![first, second])
                .await
        });

        sleep(Duration::from_millis(100)).await;
        assert!(
            !dispatch_task.is_finished(),
            "dispatch task should be backpressured at capacity=1"
        );

        let late_source = source.clone();
        let late_task = tokio::spawn(async move { late_source.subscribe(settings("q2")).await });
        sleep(Duration::from_millis(100)).await;
        assert!(
            !late_task.is_finished(),
            "late subscribe should wait while delivery gate is held during backpressure"
        );

        let _first_event = timeout(Duration::from_secs(2), primary.receiver.recv()).await??;
        timeout(Duration::from_secs(2), dispatch_task).await???;
        let _second_event = timeout(Duration::from_secs(2), primary.receiver.recv()).await??;

        let mut late = timeout(Duration::from_secs(2), late_task).await???;
        assert_eq!(late.query_id, "q2");
        assert_eq!(late.source_id, "salesforce-test-source");

        let follow_up = control_event(source.id(), source.base.next_sequence());
        source.base.dispatch_event(follow_up).await?;
        let late_event = timeout(Duration::from_secs(2), late.receiver.recv()).await??;
        assert_eq!(late_event.source_id, "salesforce-test-source");

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_waiting_subscriber_does_not_poison_gate_or_registry() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = Arc::new(
            SalesforceSource::builder("salesforce-test-source")
                .with_config(test_config(spool_root.to_string_lossy().as_ref()))
                .with_dispatch_buffer_capacity(1)
                .build()?,
        );

        let mut primary = source.subscribe(settings("q1")).await?;
        let first = control_event(source.id(), source.base.next_sequence());
        let second = control_event(source.id(), source.base.next_sequence());

        let dispatch_source = source.clone();
        let dispatch_task = tokio::spawn(async move {
            let _gate = dispatch_source.delivery_gate.lock().await;
            dispatch_source
                .base
                .dispatch_events_batch(vec![first, second])
                .await
        });

        sleep(Duration::from_millis(100)).await;
        let late_source = source.clone();
        let mut blocked_subscribe =
            tokio::spawn(async move { late_source.subscribe(settings("q-cancel")).await });
        sleep(Duration::from_millis(100)).await;
        assert!(
            !blocked_subscribe.is_finished(),
            "subscriber should be waiting on delivery gate"
        );

        blocked_subscribe.abort();
        let _ = blocked_subscribe.await;

        let _first_event = timeout(Duration::from_secs(2), primary.receiver.recv()).await??;
        timeout(Duration::from_secs(2), dispatch_task).await???;
        let _second_event = timeout(Duration::from_secs(2), primary.receiver.recv()).await??;

        let resumed = source.subscribe(settings("q3")).await?;
        assert_eq!(resumed.query_id, "q3");

        let active = source.active_queries.read().await.clone();
        assert!(active.contains_key("q1"));
        assert!(active.contains_key("q3"));
        assert!(
            !active.contains_key("q-cancel"),
            "cancelled subscriber must not be registered as active"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reopen_restores_sequence_floor_from_journal_head() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let source = SalesforceSource::builder(source_id)
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;
        let store = source.ensure_store().await?;
        store
            .commit_mutations(vec![
                insert_mutation(source_id, org_id, &plan, "001000000000011AAA", "One", 1)?,
                insert_mutation(source_id, org_id, &plan, "001000000000012AAA", "Two", 2)?,
            ])
            .await?;

        drop(store);
        drop(source);

        let reopened = SalesforceSource::builder(source_id)
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;
        let _reopened_store = reopened.ensure_store().await?;

        let first_allocated = reopened.base.next_sequence();
        assert_eq!(
            first_allocated, 3,
            "reopened source must allocate sequence after durable journal head"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_handover_immediate_live_and_restart_keep_sequence_floor_correct() -> Result<()>
    {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let source = SalesforceSource::builder(source_id)
            .with_bootstrap_provider(SequencedBootstrapProvider { row_count: 5 })
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;

        let store = source.ensure_store().await?;
        let committed = store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                "001000000000001AAA",
                "Before Bootstrap",
                1,
            )?])
            .await?;
        assert_eq!(committed.len(), 1);
        assert_eq!(committed[0].sequence, 1);

        let mut first_settings = settings("q-bootstrap");
        first_settings.enable_bootstrap = true;
        let mut first_sub = source.subscribe(first_settings).await?;

        let mut bootstrap_rx = first_sub
            .bootstrap_receiver
            .take()
            .context("expected bootstrap receiver")?;
        let mut last_bootstrap_sequence = None;
        for _ in 0..5 {
            let bootstrap_event = timeout(Duration::from_secs(2), bootstrap_rx.recv())
                .await?
                .context("bootstrap channel closed before expected row count")?;
            if let Some(previous) = last_bootstrap_sequence {
                assert!(
                    bootstrap_event.sequence >= previous,
                    "bootstrap sequence should not decrease"
                );
            }
            last_bootstrap_sequence = Some(bootstrap_event.sequence);
        }

        let bootstrap_result_rx = first_sub
            .bootstrap_result_receiver
            .take()
            .context("expected bootstrap result receiver")?;
        let bootstrap_result = timeout(Duration::from_secs(2), bootstrap_result_rx)
            .await??
            .context("bootstrap result should succeed")?;
        let boundary = bootstrap_result
            .source_position
            .context("bootstrap boundary position missing")?;
        assert_eq!(LocalPosition::decode(&boundary)?.ordinal, 1);
        let max_bootstrap_sequence = last_bootstrap_sequence
            .context("bootstrap provider should have emitted bootstrap sequences")?;

        let immediate_live = store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                "001000000000002AAA",
                "Immediate Live",
                2,
            )?])
            .await?;
        assert_eq!(immediate_live.len(), 1);
        assert_eq!(
            immediate_live[0].sequence, 2,
            "live sequence follows journal ordinal, not bootstrap row count"
        );

        source
            .base
            .dispatch_events_batch(
                immediate_live
                    .iter()
                    .map(|event| event.as_ref().clone())
                    .collect(),
            )
            .await?;

        let first_live = timeout(Duration::from_secs(2), first_sub.receiver.recv()).await??;
        assert_eq!(first_live.sequence, 2);
        assert!(
            max_bootstrap_sequence > first_live.sequence,
            "bootstrap envelope sequencing is independent from live envelope sequencing"
        );
        let first_live_pos = first_live
            .source_position
            .clone()
            .context("first live event missing source_position")?;
        assert_eq!(LocalPosition::decode(&first_live_pos)?.ordinal, 2);

        drop(first_sub);
        drop(store);
        drop(source);

        let resumed_source = SalesforceSource::builder(source_id)
            .with_bootstrap_provider(SequencedBootstrapProvider { row_count: 5 })
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;
        let mut resumed_settings = settings("q-resume");
        resumed_settings.enable_bootstrap = true; // resume_from should skip bootstrap path
        resumed_settings.resume_from = Some(first_live_pos.clone());
        resumed_settings.resume_sequence = Some(first_live.sequence);

        let mut resumed_sub = resumed_source.subscribe(resumed_settings).await?;
        assert!(resumed_sub.bootstrap_receiver.is_none());
        assert!(resumed_sub.bootstrap_result_receiver.is_none());

        let resumed_floor = resumed_source.base.next_sequence();
        assert_eq!(
            resumed_floor,
            first_live.sequence + 1,
            "resume_sequence must raise SourceBase sequence floor"
        );

        let resumed_store = resumed_source.ensure_store().await?;
        let after_restart = resumed_store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                "001000000000003AAA",
                "After Restart",
                3,
            )?])
            .await?;
        assert_eq!(after_restart.len(), 1);
        assert_eq!(after_restart[0].sequence, 3);

        resumed_source
            .base
            .dispatch_events_batch(
                after_restart
                    .iter()
                    .map(|event| event.as_ref().clone())
                    .collect(),
            )
            .await?;

        let resumed_live = timeout(Duration::from_secs(2), resumed_sub.receiver.recv()).await??;
        assert_eq!(resumed_live.sequence, 3);
        let resumed_pos = resumed_live
            .source_position
            .clone()
            .context("resumed live event missing source_position")?;
        assert_eq!(LocalPosition::decode(&resumed_pos)?.ordinal, 3);

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mismatched_resume_cursor_and_sequence_are_rejected() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let source = SalesforceSource::builder(source_id)
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;
        let store = source.ensure_store().await?;

        let committed = store
            .commit_mutations(vec![
                insert_mutation(source_id, org_id, &plan, "001000000000001AAA", "Seed 1", 1)?,
                insert_mutation(source_id, org_id, &plan, "001000000000002AAA", "Seed 2", 2)?,
            ])
            .await?;
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].sequence, 1);
        assert_eq!(committed[1].sequence, 2);

        let resume_from = committed[1]
            .source_position
            .clone()
            .context("second seed event missing source_position")?;
        assert_eq!(LocalPosition::decode(&resume_from)?.ordinal, 2);

        let mut resumed_settings = settings("q-resume-100");
        resumed_settings.resume_from = Some(resume_from);
        resumed_settings.resume_sequence = Some(100);
        let error = match source.subscribe(resumed_settings).await {
            Ok(_) => anyhow::bail!("mismatched resume pair must be rejected"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("inconsistent resume checkpoint"),
            "unexpected error message: {message}"
        );
        assert!(
            message.contains("resume_sequence=100"),
            "error should include rejected resume_sequence: {message}"
        );
        assert!(
            message.contains("cursor ordinal=2"),
            "error should include cursor ordinal: {message}"
        );
        assert!(
            !source
                .active_queries
                .read()
                .await
                .contains_key("q-resume-100"),
            "rejected mismatched resume pair must not register active subscription"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_from_ordinal_zero_remains_valid_after_first_event_retention_window(
    ) -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let source = SalesforceSource::builder(source_id)
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;
        let store = source.ensure_store().await?;
        store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                "001000000000001AAA",
                "Only Row",
                1,
            )?])
            .await?;
        let epoch = store.epoch();
        drop(store);
        drop(source);

        let reopened = SalesforceSource::builder(source_id)
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;
        let mut settings = settings("q-resume-zero");
        settings.resume_from = Some(LocalPosition::new(epoch, 0).encode());
        settings.resume_sequence = Some(0);
        let mut sub = reopened.subscribe(settings).await?;

        let replayed = timeout(Duration::from_secs(2), sub.receiver.recv()).await??;
        assert_eq!(replayed.sequence, 1);
        let replayed_pos = replayed
            .source_position
            .clone()
            .context("replayed event missing source_position")?;
        assert_eq!(LocalPosition::decode(&replayed_pos)?.ordinal, 1);

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn real_bootstrap_provider_round_trips_snapshot_ticket_and_boundary() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let bootstrap_provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(spool_root.clone())
            .build()?;

        let source = SalesforceSource::builder(source_id)
            .with_bootstrap_provider(bootstrap_provider)
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;

        let store = source.ensure_store().await?;
        let committed = store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                "001000000000001AAA",
                "Bootstrap Snapshot Row",
                1,
            )?])
            .await?;
        assert_eq!(committed.len(), 1);
        assert_eq!(committed[0].sequence, 1);

        let mut bootstrap_settings = settings("q-real-bootstrap");
        bootstrap_settings.enable_bootstrap = true;
        let mut sub = source.subscribe(bootstrap_settings).await?;

        let mut bootstrap_rx = sub
            .bootstrap_receiver
            .take()
            .context("expected bootstrap receiver from production provider")?;
        let bootstrap_event = timeout(Duration::from_secs(2), bootstrap_rx.recv())
            .await?
            .context("expected bootstrap row from production provider")?;
        match &bootstrap_event.change {
            SourceChange::Insert { element } => match element {
                Element::Node { properties, .. } => {
                    assert_eq!(
                        properties.get("Name"),
                        Some(&ElementValue::String("Bootstrap Snapshot Row".into()))
                    );
                }
                other => panic!("expected bootstrap node insert, got {other:?}"),
            },
            other => panic!("expected bootstrap insert event, got {other:?}"),
        }

        let bootstrap_result_rx = sub
            .bootstrap_result_receiver
            .take()
            .context("expected bootstrap result receiver from production provider")?;
        let bootstrap_result = timeout(Duration::from_secs(2), bootstrap_result_rx)
            .await??
            .context("expected successful bootstrap result")?;
        assert_eq!(bootstrap_result.event_count, 1);
        let boundary = bootstrap_result
            .source_position
            .context("bootstrap result missing source_position boundary")?;
        let local = LocalPosition::decode(&boundary)?;
        assert_eq!(local.epoch, store.epoch());
        assert_eq!(local.ordinal, 1);

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_failure_sets_error_instead_of_leaving_starting() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;

        let error = source
            .start()
            .await
            .expect_err("start should fail when no local Salesforce harness is running");
        let message = format!("{error:#}");
        assert!(
            message.contains("failed to obtain Salesforce token")
                || message.contains("Connection refused")
                || message.contains("error sending request"),
            "unexpected start failure error: {message}"
        );
        assert_eq!(source.status().await, ComponentStatus::Error);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_rejects_running_status_with_finished_task_handle() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;

        source
            .base
            .set_status(ComponentStatus::Running, Some("running".to_string()))
            .await;

        let finished = tokio::spawn(async {});
        let _ = finished.await;
        let stale_handle = tokio::spawn(async {});
        *source.base.task_handle.write().await = Some(stale_handle);
        if let Some(handle) = source.base.task_handle.read().await.as_ref() {
            while !handle.is_finished() {
                sleep(Duration::from_millis(5)).await;
            }
        }

        let error = source
            .start()
            .await
            .expect_err("start must reject Running status when runtime task is dead");
        assert!(
            format!("{error:#}").contains("runtime task is no longer active"),
            "unexpected error: {error:#}"
        );
        assert_eq!(source.status().await, ComponentStatus::Error);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_rejects_when_previous_runtime_task_is_still_alive() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;

        source
            .base
            .set_status(ComponentStatus::Error, Some("error".to_string()))
            .await;
        let task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        *source.base.task_handle.write().await = Some(task);

        let error = source
            .start()
            .await
            .expect_err("start must reject while prior runtime task is alive");
        assert!(
            format!("{error:#}").contains("prior runtime task is still active"),
            "unexpected error: {error:#}"
        );

        if let Some(handle) = source.base.task_handle.write().await.take() {
            let mut handle = handle;
            handle.abort();
            let _ = timeout(Duration::from_secs(2), &mut handle).await;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_subscription_releases_admission_quota() -> Result<()> {
        let temp = TempDir::new()?;
        let mut config = test_config(temp.path().join("spool").to_string_lossy().as_ref());
        config.max_subscribers = 1;
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(config)
            .build()?;
        let first = source
            .subscribe(settings("cancelled-after-success"))
            .await?;
        assert!(source.subscribe(settings("while-full")).await.is_err());
        drop(first);
        let second = source.subscribe(settings("replacement")).await?;
        assert_eq!(source.active_queries.read().await.len(), 1);
        drop(second);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_retained_journal_rejects_expired_predecessor_with_typed_boundary() -> Result<()>
    {
        let temp = TempDir::new()?;
        let config = test_config(temp.path().join("spool").to_string_lossy().as_ref());
        let source_id = "salesforce-test-source";
        let source = SalesforceSource::builder(source_id)
            .with_config(config.clone())
            .build()?;
        let store = source.ensure_store().await?;
        let beginning = LocalPosition::new(store.epoch(), 0).encode();
        assert_eq!(
            source.validate_resume_position(&store, &beginning).await?,
            0
        );
        let change = insert_mutation(
            source_id,
            &config.connection.expected_org_id,
            &ObjectPlan::account(),
            "001000000000001AAA",
            "Expired journal row",
            1,
        )?;
        store.commit_mutations(vec![change]).await?;
        store.expire_unpinned_events().await?;
        assert_eq!(store.head_ordinal().await?, 1);
        assert!(store.read_events_page(1, 1, 1, 1024 * 1024).await.is_err());
        let mut expired = settings("expired-predecessor");
        expired.resume_from = Some(beginning.clone());
        let error = match source.subscribe(expired).await {
            Ok(_) => anyhow::bail!("expired predecessor must fail at subscription"),
            Err(error) => error,
        };
        let head = LocalPosition::new(store.epoch(), 1).encode();
        match error.downcast_ref::<SourceError>() {
            Some(SourceError::PositionUnavailable {
                requested,
                earliest_available,
                ..
            }) => {
                assert_eq!(requested, &beginning);
                assert_eq!(earliest_available.as_ref(), Some(&head));
            }
            _ => anyhow::bail!("expected typed PositionUnavailable, got {error:#}"),
        }
        let mut current = settings("current-head");
        current.resume_from = Some(head);
        let _subscription = source.subscribe(current).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nonempty_bootstrap_sentinel_is_resumable_and_missing_positions_are_rejected(
    ) -> Result<()> {
        let temp = TempDir::new()?;
        let config = test_config(temp.path().join("spool").to_string_lossy().as_ref());
        let source_id = "salesforce-test-source";
        let source = SalesforceSource::builder(source_id)
            .with_config(config.clone())
            .build()?;
        let store = source.ensure_store().await?;
        store
            .commit_mutations(vec![insert_mutation(
                source_id,
                &config.connection.expected_org_id,
                &ObjectPlan::account(),
                "001000000000001AAA",
                "Snapshot row",
                1,
            )?])
            .await?;
        let boundary = LocalPosition::new(store.epoch(), 1).encode();
        drop(store);
        drop(source);

        let reopened = SalesforceSource::builder(source_id)
            .with_config(config)
            .build()?;
        let mut resume = settings("bootstrap-restart");
        resume.resume_from = Some(boundary);
        resume.resume_sequence = Some(0);
        let subscription = reopened.subscribe(resume).await?;
        assert!(subscription.bootstrap_receiver.is_none());
        let mut missing = settings("missing-position");
        missing.resume_sequence = Some(100);
        assert!(reopened.subscribe(missing).await.is_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bootstrap_subscription_protects_history_until_live_checkpoint_confirmation(
    ) -> Result<()> {
        let temp = TempDir::new()?;
        let spool = temp.path().join("spool");
        let mut config = test_config(spool.to_string_lossy().as_ref());
        config.max_events = 3;
        let source_id = "salesforce-test-source";
        let source = SalesforceSource::builder(source_id)
            .with_config(config.clone())
            .with_bootstrap_provider(
                SalesforceBootstrapProvider::builder()
                    .with_spool_root(&spool)
                    .build()?,
            )
            .build()?;
        let store = source.ensure_store().await?;
        let change = |ordinal| {
            insert_mutation(
                source_id,
                &config.connection.expected_org_id,
                &ObjectPlan::account(),
                &format!("0010000000000{ordinal:02}AAA"),
                "Snapshot/tail",
                ordinal,
            )
        };
        store.commit_mutations(vec![change(1)?]).await?;
        let mut options = settings("slow-bootstrap");
        options.enable_bootstrap = true;
        options.request_position_handle = true;
        let subscription = source.subscribe(options).await?;
        assert!(subscription.bootstrap_receiver.is_some());
        let confirmed = subscription
            .position_handle
            .as_ref()
            .context("missing confirmation handle")?;
        store
            .commit_mutations(vec![change(2)?, change(3)?, change(4)?])
            .await?;
        assert!(store.commit_mutations(vec![change(5)?]).await.is_err());
        confirmed.store(0, Ordering::Release);
        assert!(store.commit_mutations(vec![change(5)?]).await.is_err());
        assert_eq!(store.read_events_page(2, 4, 3, 1024 * 1024).await?.len(), 3);
        confirmed.store(4, Ordering::Release);
        store
            .commit_mutations(vec![change(5)?, change(6)?, change(7)?])
            .await?;
        assert_eq!(store.earliest_resumable_ordinal().await?, 4);
        drop(subscription);
        source.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_panic_is_reported_as_component_error() -> Result<()> {
        let temp = TempDir::new()?;
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(test_config(
                temp.path().join("spool").to_string_lossy().as_ref(),
            ))
            .build()?;
        source.base.set_status(ComponentStatus::Running, None).await;
        spawn_runtime_task(
            source.id().into(),
            source.base.clone_shared(),
            std::future::poll_fn(|_| -> std::task::Poll<Result<()>> {
                panic!("simulated runtime panic")
            }),
            tracing::info_span!("runtime_panic_test"),
        )
        .await?;
        assert_eq!(source.status().await, ComponentStatus::Error);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn regression_stop_joins_cancelled_start_initializer() -> Result<()> {
        let temp = TempDir::new()?;
        let source = Arc::new(
            SalesforceSource::builder("salesforce-test-source")
                .with_config(test_config(temp.path().to_string_lossy().as_ref()))
                .build()?,
        );
        let store = source.ensure_store().await?;
        *source.store.write().await = None;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let initializing = source.clone();
        let waiter = tokio::spawn(async move {
            initializing
                .get_or_initialize_store(async move {
                    let _ = entered_tx.send(());
                    release_rx.await?;
                    Ok(store)
                })
                .await
        });
        entered_rx.await?;
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));

        let stopping = source.clone();
        let cancelled_stop = tokio::spawn(async move { stopping.stop().await });
        tokio::task::yield_now().await;
        assert!(
            !cancelled_stop.is_finished(),
            "stop must join the initializer"
        );
        cancelled_stop.abort();
        assert!(cancelled_stop.await.unwrap_err().is_cancelled());
        let error = source
            .stop()
            .await
            .expect_err("blocked initialization must time out");
        assert!(format!("{error:#}").contains("initialization"), "{error:#}");
        assert_eq!(source.status().await, ComponentStatus::Error);
        assert!(source.store_initializer.lock().await.is_some());
        let error = source
            .start()
            .await
            .expect_err("restart must retain initializer ownership");
        assert!(format!("{error:#}").contains("initializ"), "{error:#}");

        release_tx
            .send(())
            .map_err(|_| anyhow!("initializer was detached"))?;
        source.stop().await?;
        assert_eq!(source.status().await, ComponentStatus::Stopped);
        assert!(source.store_initializer.lock().await.is_none());
        assert!(source.store.read().await.is_some());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_initializer_waiter_preserves_the_open_database_and_publication(
    ) -> Result<()> {
        let temp = TempDir::new()?;
        let source = Arc::new(
            SalesforceSource::builder("salesforce-test-source")
                .with_config(test_config(
                    temp.path().join("spool").to_string_lossy().as_ref(),
                ))
                .build()?,
        );
        let store = source.ensure_store().await?;
        *source.store.write().await = None;
        let original = Arc::as_ptr(&store) as usize;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let first_source = source.clone();
        let first = tokio::spawn(async move {
            first_source
                .get_or_initialize_store(async move {
                    tokio::task::spawn_blocking(move || {
                        let _ = entered_tx.send(());
                        release_rx
                            .blocking_recv()
                            .context("initialization gate closed")?;
                        Ok(store)
                    })
                    .await?
                })
                .await
        });
        entered_rx.await?;
        first.abort();
        assert!(matches!(first.await, Err(error) if error.is_cancelled()));
        assert!(source.store_initializer.lock().await.is_some());
        let second_source = source.clone();
        let second = tokio::spawn(async move { second_source.ensure_store().await });
        release_tx
            .send(())
            .map_err(|_| anyhow!("initializer lost ownership"))?;
        let initialized = timeout(Duration::from_secs(2), second).await???;
        assert_eq!(Arc::as_ptr(&initialized) as usize, original);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_stop_and_deprovision_cannot_outlive_a_blocking_writer() -> Result<()> {
        let temp = TempDir::new()?;
        let spool = temp.path().join("spool");
        let config = test_config(spool.to_string_lossy().as_ref());
        let source_id = "salesforce-test-source";
        let source = SalesforceSource::builder(source_id)
            .with_config(config.clone())
            .build()?;
        let store = source.ensure_store().await?;
        let published = SourceSchema {
            nodes: vec![drasi_lib::schema::NodeSchema::new("Published")],
            relations: Vec::new(),
        };
        source
            .schema_snapshot
            .send_replace(Some(Arc::new(published.clone())));
        let schema_snapshot = source.schema_snapshot.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let finished = completed.clone();
        let writer_store = store.clone();
        *source.base.task_handle.write().await = Some(tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                let _ = release_rx.blocking_recv();
            })
            .await
            .expect("blocking writer");
            writer_store
                .commit_mutations(vec![insert_mutation(
                    source_id,
                    &config.connection.expected_org_id,
                    &ObjectPlan::account(),
                    "001000000000001AAA",
                    "Committed before stop",
                    1,
                )
                .expect("mutation")])
                .await
                .expect("commit");
            schema_snapshot.send_replace(Some(Arc::new(SourceSchema {
                nodes: vec![drasi_lib::schema::NodeSchema::new("AfterWriter")],
                relations: Vec::new(),
            })));
            finished.store(true, Ordering::Release);
        }));
        entered_rx.await?;
        assert!(timeout(Duration::from_millis(10), source.stop())
            .await
            .is_err());
        assert!(
            source.base.task_handle.read().await.is_some(),
            "cancelled stop lost ownership"
        );
        assert!(
            source.deprovision().await.is_err(),
            "active writer must prevent cleanup"
        );
        assert!(spool.join("mirror.redb").exists());
        assert_eq!(source.describe_schema(), Some(published));
        release_tx
            .send(())
            .map_err(|_| anyhow!("writer ended before release"))?;
        timeout(Duration::from_secs(2), async {
            while !completed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        source.stop().await?;
        assert_eq!(store.head_ordinal().await?, 1);
        assert_eq!(source.status().await, ComponentStatus::Stopped);
        assert_eq!(
            source.describe_schema().unwrap().nodes[0].label,
            "AfterWriter"
        );
        source.deprovision().await?;
        assert_eq!(source.describe_schema(), None);
        assert!(!spool.join("mirror.redb").exists());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_ensure_store_initialization_returns_shared_store() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = StdArc::new(
            SalesforceSource::builder("salesforce-test-source")
                .with_config(test_config(spool_root.to_string_lossy().as_ref()))
                .build()?,
        );

        let mut joins = Vec::new();
        for _ in 0..8 {
            let source = source.clone();
            joins.push(tokio::spawn(async move { source.ensure_store().await }));
        }

        let mut stores = Vec::new();
        for join in joins {
            stores.push(join.await??);
        }

        let first_ptr = Arc::as_ptr(&stores[0]);
        assert!(
            stores.iter().all(|store| Arc::as_ptr(store) == first_ptr),
            "all concurrent ensure_store calls must return the same shared store instance"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_task_failures_transition_source_status_to_error() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;

        source
            .base
            .set_status(ComponentStatus::Running, Some("running".to_string()))
            .await;

        let task = spawn_runtime_task(
            "salesforce-test-source".to_string(),
            source.base.clone_shared(),
            async { Err(anyhow!("boom")) },
            tracing::info_span!("runtime_failure_status_test"),
        );
        *source.base.task_handle.write().await = Some(task);

        timeout(Duration::from_secs(2), async {
            loop {
                if source.status().await == ComponentStatus::Error {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        assert_eq!(source.status().await, ComponentStatus::Error);
        if let Some(handle) = source.base.task_handle.write().await.take() {
            let _ = handle.await;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_timeout_preserves_runtime_task_handle_and_blocks_restart() -> Result<()> {
        let temp = TempDir::new()?;
        let spool_root = temp.path().join("spool");
        let source = SalesforceSource::builder("salesforce-test-source")
            .with_config(test_config(spool_root.to_string_lossy().as_ref()))
            .build()?;

        source
            .base
            .set_status(ComponentStatus::Running, Some("running".to_string()))
            .await;

        let started = StdArc::new(AtomicBool::new(false));
        let started_clone = started.clone();
        let task = tokio::spawn(async move {
            started_clone.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        *source.base.task_handle.write().await = Some(task);

        let error = timeout(Duration::from_secs(20), source.stop())
            .await
            .context("stop call timed out in test")?
            .expect_err("stop must fail instead of detaching a timed-out runtime task");
        assert!(started.load(Ordering::SeqCst));
        assert!(
            format!("{error:#}").contains("remains attached"),
            "unexpected stop timeout error: {error:#}"
        );
        assert_eq!(source.status().await, ComponentStatus::Error);
        assert!(
            source.base.task_handle.read().await.is_some(),
            "timed-out stop must preserve runtime task ownership"
        );

        let restart_error = timeout(Duration::from_secs(2), source.start())
            .await
            .context("start call timed out in test")?
            .expect_err("start must reject while timed-out runtime task is still alive");
        assert!(
            format!("{restart_error:#}").contains("prior runtime task is still active"),
            "unexpected restart rejection error: {restart_error:#}"
        );

        if let Some(handle) = source.base.task_handle.write().await.take() {
            let mut handle = handle;
            handle.abort();
            let _ = timeout(Duration::from_secs(2), &mut handle).await;
        }
        Ok(())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "salesforce-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::SalesforceSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
