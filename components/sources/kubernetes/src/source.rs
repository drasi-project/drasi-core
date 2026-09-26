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

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use drasi_kubernetes_common::config::{
    is_cluster_scoped_kind, AuthMode, KubernetesSourceConfig, ResourceSpec, StartFrom,
};
use drasi_kubernetes_common::mapping::{
    build_delete_changes, build_insert_changes, build_update_changes, extract_uid,
    object_created_at_millis,
};
use drasi_kubernetes_common::{build_client, parse_api_version};
use drasi_lib::channels::{ComponentStatus, DispatchMode, SubscriptionResponse};
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{BootstrapProvider, Source};
use futures::stream::{BoxStream, SelectAll};
use futures::StreamExt;
use kube::api::{Api, DynamicObject, ListParams, WatchParams};
use kube::core::{ApiResource, GroupVersionKind};
use kube::runtime::watcher::{self, Event};
use kube::Client;
use log::{debug, error, info, warn};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::Instrument;

const SEEN_UIDS_KEY: &str = "seen_uids";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct KubernetesSource {
    pub(crate) base: SourceBase,
    config: KubernetesSourceConfig,
    state_store_override: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
}

impl KubernetesSource {
    pub fn builder(id: impl Into<String>) -> KubernetesSourceBuilder {
        KubernetesSourceBuilder::new(id)
    }

    pub fn new(id: impl Into<String>, config: KubernetesSourceConfig) -> Result<Self> {
        config.validate()?;
        let params = SourceBaseParams::new(id.into());
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            state_store_override: Arc::new(RwLock::new(None)),
        })
    }

    async fn resolve_state_store(&self) -> Option<Arc<dyn StateStoreProvider>> {
        if let Some(store) = self.state_store_override.read().await.clone() {
            return Some(store);
        }
        self.base.state_store().await
    }
}

#[async_trait]
impl Source for KubernetesSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "kubernetes"
    }

    fn properties(&self) -> HashMap<String, Value> {
        use crate::descriptor::KubernetesSourceConfigDto;

        self.base
            .properties_or_serialize(&KubernetesSourceConfigDto::from(&self.config))
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        let mut task_handle = self.base.task_handle.write().await;
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }
        anyhow::ensure!(
            task_handle.is_none(),
            "Kubernetes source '{}' must be stopped before restarting",
            self.base.id
        );

        let source_id = self.base.id.clone();
        self.base.set_status(ComponentStatus::Starting, None).await;
        info!("Starting Kubernetes source '{source_id}'");

        let initialization = tokio::time::timeout(STARTUP_TIMEOUT, async {
            self.config.validate()?;
            let client = build_client(&self.config).await?;
            validate_resource_access(&client, &self.config).await?;
            Ok::<_, anyhow::Error>(client)
        })
        .await
        .context("Kubernetes source initialization timed out after 10 seconds")
        .and_then(|result| result);
        let client = match initialization {
            Ok(client) => client,
            Err(error) => {
                error!("Failed to start Kubernetes source '{source_id}': {error:#}");
                self.base
                    .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                    .await;
                return Err(error);
            }
        };

        let config = self.config.clone();
        let base = self.base.clone_shared();
        let status_handle = self.base.status_handle();
        let state_store = self.resolve_state_store().await;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "kubernetes_source_task",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Kubernetes source started".to_string()),
            )
            .await;
        let task = tokio::spawn(
            async move {
                let run_result =
                    run_source_stream(&source_id, config, client, base, state_store, shutdown_rx)
                        .await;

                if let Err(e) = run_result {
                    error!("Kubernetes source task failed for '{source_id}': {e}");
                    status_handle
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Kubernetes source task failed: {e}")),
                        )
                        .await;
                } else {
                    status_handle
                        .set_status(ComponentStatus::Stopped, None)
                        .await;
                }
            }
            .instrument(span),
        );

        *task_handle = Some(task);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let mut task_handle = self.base.task_handle.write().await;
        if self.base.get_status().await != ComponentStatus::Stopped {
            self.base.set_status(ComponentStatus::Stopping, None).await;
        }
        if let Some(shutdown) = self.base.shutdown_tx.write().await.take() {
            let _ = shutdown.send(());
        }

        let mut task_error = None;
        if let Some(mut task) = task_handle.take() {
            let result = match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
                Ok(result) => result,
                Err(_) => {
                    warn!(
                        "Kubernetes source '{}' did not stop within 5 seconds; aborting",
                        self.base.id
                    );
                    task.abort();
                    task.await
                }
            };
            if let Err(error) = result {
                if !error.is_cancelled() {
                    task_error = Some(anyhow::Error::new(error));
                }
            }
        }
        self.base.clear_dispatchers().await;

        if let Some(error) = task_error {
            let error = error.context(format!(
                "Kubernetes source '{}' task failed during shutdown",
                self.base.id
            ));
            error!("{error:#}");
            self.base
                .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                .await;
            return Err(error);
        }
        anyhow::ensure!(
            self.base.get_status().await != ComponentStatus::Error,
            "Kubernetes source '{}' failed while shutting down; see the component error",
            self.base.id
        );
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Kubernetes source stopped".to_string()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "Kubernetes")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

pub struct KubernetesSourceBuilder {
    id: String,
    config: KubernetesSourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    auto_start: bool,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl KubernetesSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: KubernetesSourceConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
            state_store: None,
        }
    }

    pub fn with_config(mut self, config: KubernetesSourceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_resources(mut self, resources: Vec<ResourceSpec>) -> Self {
        self.config.resources = resources;
        self
    }

    pub fn with_namespaces(mut self, namespaces: Vec<String>) -> Self {
        self.config.namespaces = namespaces;
        self
    }

    pub fn with_label_selector(mut self, selector: impl Into<String>) -> Self {
        self.config.label_selector = Some(selector.into());
        self
    }

    pub fn with_field_selector(mut self, selector: impl Into<String>) -> Self {
        self.config.field_selector = Some(selector.into());
        self
    }

    pub fn with_auth_mode(mut self, auth_mode: AuthMode) -> Self {
        self.config.auth_mode = auth_mode;
        self
    }

    pub fn with_kubeconfig_path(mut self, path: impl Into<String>) -> Self {
        self.config.kubeconfig_path = Some(path.into());
        self
    }

    pub fn with_kubeconfig_content(mut self, content: impl Into<String>) -> Self {
        self.config.kubeconfig_content = Some(content.into());
        self
    }

    pub fn with_include_owner_relations(mut self, include: bool) -> Self {
        self.config.include_owner_relations = include;
        self
    }

    pub fn with_start_from(mut self, start_from: StartFrom) -> Self {
        self.config.start_from = start_from;
        self
    }

    pub fn with_exclude_annotations(mut self, exclude_annotations: Vec<String>) -> Self {
        self.config.exclude_annotations = exclude_annotations;
        self
    }

    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_bootstrap_provider(mut self, provider: impl BootstrapProvider + 'static) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
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

    pub fn build(self) -> Result<KubernetesSource> {
        self.config.validate()?;

        let mut params = SourceBaseParams::new(self.id);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }
        params = params.with_auto_start(self.auto_start);

        Ok(KubernetesSource {
            base: SourceBase::new(params)?,
            config: self.config,
            state_store_override: Arc::new(RwLock::new(self.state_store)),
        })
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct WatchTarget {
    api_version: String,
    kind: String,
    namespace: Option<String>,
}

impl WatchTarget {
    fn key(&self) -> String {
        let namespace = self.namespace.as_deref().unwrap_or("_all");
        let api_version = &self.api_version;
        let kind = &self.kind;
        format!("{api_version}:{kind}:{namespace}")
    }
}

fn build_watch_targets(config: &KubernetesSourceConfig) -> Vec<WatchTarget> {
    let mut targets = Vec::new();
    for res in &config.resources {
        if is_cluster_scoped_kind(&res.kind) {
            targets.push(WatchTarget {
                api_version: res.api_version.clone(),
                kind: res.kind.clone(),
                namespace: None,
            });
            continue;
        }

        if config.namespaces.is_empty() {
            targets.push(WatchTarget {
                api_version: res.api_version.clone(),
                kind: res.kind.clone(),
                namespace: None,
            });
            continue;
        }

        for ns in &config.namespaces {
            targets.push(WatchTarget {
                api_version: res.api_version.clone(),
                kind: res.kind.clone(),
                namespace: Some(ns.clone()),
            });
        }
    }
    targets
}

fn target_api(client: Client, target: &WatchTarget) -> Result<Api<DynamicObject>> {
    let (group, version) = parse_api_version(&target.api_version)?;
    let gvk = GroupVersionKind::gvk(&group, &version, &target.kind);
    let api_resource = ApiResource::from_gvk(&gvk);
    Ok(match &target.namespace {
        Some(namespace) => Api::namespaced_with(client, namespace, &api_resource),
        None => Api::all_with(client, &api_resource),
    })
}

fn is_fatal_api_error(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if is_fatal_status(response.code))
}

fn is_fatal_status(code: u16) -> bool {
    matches!(code, 400 | 401 | 403 | 404 | 422)
}

fn is_fatal_watch_error(error: &watcher::Error) -> bool {
    match error {
        watcher::Error::InitialListFailed(error)
        | watcher::Error::WatchStartFailed(error)
        | watcher::Error::WatchFailed(error) => is_fatal_api_error(error),
        watcher::Error::WatchError(response) => is_fatal_status(response.code),
        _ => false,
    }
}

async fn validate_resource_access(client: &Client, config: &KubernetesSourceConfig) -> Result<()> {
    for target in build_watch_targets(config) {
        let api = target_api(client.clone(), &target)?;
        let mut list_params = ListParams::default().limit(1);
        let mut watch_params = WatchParams::default().timeout(5);
        if let Some(selector) = &config.label_selector {
            list_params = list_params.labels(selector);
            watch_params = watch_params.labels(selector);
        }
        if let Some(selector) = &config.field_selector {
            list_params = list_params.fields(selector);
            watch_params = watch_params.fields(selector);
        }

        loop {
            let result = async {
                let list = api
                    .list(&list_params)
                    .await
                    .with_context(|| format!("Cannot list Kubernetes resource {}", target.key()))?;
                let resource_version = list
                    .metadata
                    .resource_version
                    .context("Kubernetes list response has no resourceVersion")?;
                // List and watch are distinct RBAC permissions. Check both before Running.
                // Api::watch defers HTTP errors to the first stream item. Inspect
                // the response headers without waiting for a resource event.
                let request = kube::core::Request::new(api.resource_url())
                    .watch(&watch_params, &resource_version)?;
                let response = client
                    .send(request.map(kube::client::Body::from))
                    .await
                    .with_context(|| {
                        format!("Cannot watch Kubernetes resource {}", target.key())
                    })?;
                if !response.status().is_success() {
                    let status = response.status();
                    let bytes = response.into_body().collect_bytes().await?;
                    let mut response =
                        match serde_json::from_slice::<kube::error::ErrorResponse>(&bytes) {
                            Ok(response) => response,
                            Err(error) => {
                                warn!(
                                    "Non-JSON Kubernetes watch error response ({status}): {error}"
                                );
                                kube::error::ErrorResponse {
                                    status: "Failure".to_string(),
                                    message: String::from_utf8_lossy(&bytes).into_owned(),
                                    reason: status.to_string(),
                                    code: status.as_u16(),
                                }
                            }
                        };
                    response.code = status.as_u16();
                    return Err(kube::Error::Api(response)).with_context(|| {
                        format!("Cannot watch Kubernetes resource {}", target.key())
                    });
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            match result {
                Ok(()) => break,
                Err(error)
                    if error
                        .downcast_ref::<kube::Error>()
                        .is_some_and(|error| !is_fatal_api_error(error)) =>
                {
                    warn!("Retrying Kubernetes resource access: {error:#}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

async fn run_source_stream(
    source_id: &str,
    config: KubernetesSourceConfig,
    client: Client,
    base: SourceBase,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let targets = build_watch_targets(&config);

    let target_count = targets.len();
    info!("Kubernetes source '{source_id}' watching {target_count} target streams");

    let mut streams: SelectAll<
        BoxStream<
            'static,
            (
                WatchTarget,
                std::result::Result<Event<DynamicObject>, watcher::Error>,
            ),
        >,
    > = SelectAll::new();
    let mut init_done: HashMap<String, bool> = HashMap::new();
    for target in &targets {
        init_done.insert(target.key(), !matches!(config.start_from, StartFrom::Now));

        let api = target_api(client.clone(), target)?;

        let mut watch_cfg = watcher::Config::default();
        if let Some(label_selector) = &config.label_selector {
            watch_cfg = watch_cfg.labels(label_selector);
        }
        if let Some(field_selector) = &config.field_selector {
            watch_cfg = watch_cfg.fields(field_selector);
        }

        let target_clone = target.clone();
        let stream = watcher::watcher(api, watch_cfg)
            .map(move |event| (target_clone.clone(), event))
            .boxed();
        streams.push(stream);
    }

    let mut seen_uids = load_seen_uids(source_id, &state_store).await?;

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown_rx => {
                info!("Kubernetes source '{source_id}' received shutdown signal");
                break;
            }
            maybe_event = streams.next() => {
                let Some((target, event_result)) = maybe_event else {
                    warn!("Kubernetes watcher stream ended unexpectedly for source '{source_id}'");
                    break;
                };

                let target_key = target.key();
                match event_result {
                    Ok(event) => {
                        match event {
                            Event::Init => {
                                debug!("{target_key} INIT");
                                if matches!(config.start_from, StartFrom::Now) {
                                    init_done.insert(target_key, false);
                                }
                            }
                            Event::InitApply(obj) => {
                                if matches!(config.start_from, StartFrom::Now) {
                                    // Track UID so post-init Apply events are classified as updates
                                    if let Some(uid) = extract_uid(&obj) {
                                        seen_uids.insert(uid);
                                    }
                                    continue;
                                }
                                if let StartFrom::Timestamp(ts) = config.start_from {
                                    if let Some(created_at) = object_created_at_millis(&obj) {
                                        if created_at < ts {
                                            // Track UID so post-init Apply events are classified as updates
                                            if let Some(uid) = extract_uid(&obj) {
                                                seen_uids.insert(uid);
                                            }
                                            continue;
                                        }
                                    }
                                }
                                // Dispatched InitApply: let process_apply_object decide insert vs update
                                process_apply_object(
                                    source_id,
                                    &config,
                                    &target,
                                    obj,
                                    &base,
                                    &mut seen_uids,
                                ).await?;
                            }
                            Event::InitDone => {
                                debug!("{target_key} INIT_DONE");
                                init_done.insert(target_key, true);
                                // Checkpoint: persist seen_uids after full init list
                                save_seen_uids(source_id, &state_store, &seen_uids).await?;
                            }
                            Event::Apply(obj) => {
                                if matches!(config.start_from, StartFrom::Now) && !init_done.get(&target_key).copied().unwrap_or(false) {
                                    continue;
                                }
                                process_apply_object(
                                    source_id,
                                    &config,
                                    &target,
                                    obj,
                                    &base,
                                    &mut seen_uids,
                                ).await?;
                            }
                            Event::Delete(obj) => {
                                if matches!(config.start_from, StartFrom::Now) && !init_done.get(&target_key).copied().unwrap_or(false) {
                                    continue;
                                }
                                if let Some(uid) = extract_uid(&obj) {
                                    seen_uids.remove(&uid);
                                }
                                let changes = build_delete_changes(source_id, &target.kind, &obj, &config)?;
                                dispatch_changes(source_id, &base, changes).await?;
                            }
                        }
                    }
                    Err(e) => {
                        if is_fatal_watch_error(&e) {
                            return Err(e).with_context(|| {
                                format!("Fatal Kubernetes watcher error for {target_key}")
                            });
                        }
                        warn!("Kubernetes watcher error for {target_key}: {e}");
                    }
                }
            }
        }
    }

    // Checkpoint: persist seen_uids on graceful shutdown
    save_seen_uids(source_id, &state_store, &seen_uids).await?;

    Ok(())
}

async fn process_apply_object(
    source_id: &str,
    config: &KubernetesSourceConfig,
    target: &WatchTarget,
    obj: DynamicObject,
    base: &SourceBase,
    seen_uids: &mut HashSet<String>,
) -> Result<()> {
    let uid = extract_uid(&obj).ok_or_else(|| anyhow!("Apply event missing metadata.uid"))?;
    let is_insert = !seen_uids.contains(&uid);
    let changes = if is_insert {
        seen_uids.insert(uid.clone());
        build_insert_changes(source_id, &target.kind, &obj, config)?
    } else {
        build_update_changes(source_id, &target.kind, &obj, config)?
    };

    dispatch_changes(source_id, base, changes).await?;
    Ok(())
}

pub(crate) async fn dispatch_changes(
    _source_id: &str,
    base: &SourceBase,
    changes: Vec<drasi_core::models::SourceChange>,
) -> Result<()> {
    for change in changes {
        base.dispatch_source_change(change).await?;
    }
    Ok(())
}

async fn load_seen_uids(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<HashSet<String>> {
    let Some(store) = state_store else {
        return Ok(HashSet::new());
    };
    let raw = store
        .get(source_id, SEEN_UIDS_KEY)
        .await
        .map_err(|e| anyhow!("StateStore get '{SEEN_UIDS_KEY}' failed: {e}"))?;
    let Some(bytes) = raw else {
        return Ok(HashSet::new());
    };
    let parsed: Vec<String> = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("Failed to parse seen_uids state: {e}"))?;
    Ok(parsed.into_iter().collect())
}

async fn save_seen_uids(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    seen_uids: &HashSet<String>,
) -> Result<()> {
    let Some(store) = state_store else {
        return Ok(());
    };
    let mut sorted = seen_uids.iter().cloned().collect::<Vec<_>>();
    sorted.sort_unstable();
    let bytes = serde_json::to_vec(&sorted)?;
    store
        .set(source_id, SEEN_UIDS_KEY, bytes)
        .await
        .map_err(|e| anyhow!("StateStore set '{SEEN_UIDS_KEY}' failed: {e}"))?;
    Ok(())
}
