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

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_source_kubernetes::mapping::build_insert_changes;
use drasi_source_kubernetes::{AuthMode, KubernetesSourceConfig, ResourceSpec};
use kube::api::{Api, DynamicObject, ListParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::core::{ApiResource, GroupVersionKind};
use kube::{Client, Config};
use log::info;

use crate::config::KubernetesBootstrapConfig;

pub struct KubernetesBootstrapProvider {
    _config: KubernetesBootstrapConfig,
    source_config: KubernetesSourceConfig,
}

impl KubernetesBootstrapProvider {
    pub fn new(config: KubernetesBootstrapConfig, source_config: KubernetesSourceConfig) -> Self {
        Self {
            _config: config,
            source_config,
        }
    }

    pub fn builder() -> KubernetesBootstrapProviderBuilder {
        KubernetesBootstrapProviderBuilder::new()
    }
}

pub struct KubernetesBootstrapProviderBuilder {
    config: KubernetesBootstrapConfig,
    source_config: Option<KubernetesSourceConfig>,
}

impl KubernetesBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            config: KubernetesBootstrapConfig::default(),
            source_config: None,
        }
    }

    pub fn with_source_config(mut self, source_config: KubernetesSourceConfig) -> Self {
        self.source_config = Some(source_config);
        self
    }

    pub fn with_config(mut self, config: KubernetesBootstrapConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> Result<KubernetesBootstrapProvider> {
        let source_config = self
            .source_config
            .ok_or_else(|| anyhow!("Kubernetes source configuration is required for bootstrap"))?;
        source_config.validate()?;
        Ok(KubernetesBootstrapProvider::new(self.config, source_config))
    }
}

impl Default for KubernetesBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BootstrapProvider for KubernetesBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        let client = build_client(&self.source_config).await?;
        let targets = build_bootstrap_targets(&self.source_config);
        let mut total_events = 0usize;

        info!(
            "Starting Kubernetes bootstrap for query '{}' with {} targets",
            request.query_id,
            targets.len()
        );

        for target in targets {
            let (group, version) = parse_api_version(&target.resource.api_version)?;
            let gvk = GroupVersionKind::gvk(&group, &version, &target.resource.kind);
            let api_resource = ApiResource::from_gvk(&gvk);
            let api: Api<DynamicObject> = match &target.namespace {
                Some(namespace) => Api::namespaced_with(client.clone(), namespace, &api_resource),
                None => Api::all_with(client.clone(), &api_resource),
            };

            let mut list_params = ListParams::default();
            if let Some(label_selector) = &self.source_config.label_selector {
                list_params = list_params.labels(label_selector);
            }
            if let Some(field_selector) = &self.source_config.field_selector {
                list_params = list_params.fields(field_selector);
            }

            let list = api.list(&list_params).await?;
            for obj in list.items {
                let changes = build_insert_changes(
                    &context.source_id,
                    &target.resource.kind,
                    &obj,
                    &self.source_config,
                )?;
                for change in changes {
                    if !matches_labels(&request, &change) {
                        continue;
                    }

                    let event = BootstrapEvent {
                        source_id: context.source_id.clone(),
                        change,
                        timestamp: chrono::Utc::now(),
                        sequence: context.next_sequence(),
                    };
                    event_tx
                        .send(event)
                        .await
                        .map_err(|e| anyhow!("Failed to send Kubernetes bootstrap event: {e}"))?;
                    total_events += 1;
                }
            }
        }

        info!(
            "Completed Kubernetes bootstrap for query '{}': sent {} events",
            request.query_id, total_events
        );
        Ok(total_events)
    }
}

struct BootstrapTarget {
    resource: ResourceSpec,
    namespace: Option<String>,
}

fn build_bootstrap_targets(config: &KubernetesSourceConfig) -> Vec<BootstrapTarget> {
    let mut targets = Vec::new();
    for resource in &config.resources {
        if is_cluster_scoped_kind(&resource.kind) || config.namespaces.is_empty() {
            targets.push(BootstrapTarget {
                resource: resource.clone(),
                namespace: None,
            });
            continue;
        }

        for namespace in &config.namespaces {
            targets.push(BootstrapTarget {
                resource: resource.clone(),
                namespace: Some(namespace.clone()),
            });
        }
    }
    targets
}

fn is_cluster_scoped_kind(kind: &str) -> bool {
    matches!(kind, "Node" | "Namespace")
}

fn parse_api_version(api_version: &str) -> Result<(String, String)> {
    if let Some((group, version)) = api_version.split_once('/') {
        if group.is_empty() || version.is_empty() {
            return Err(anyhow!("Invalid apiVersion '{api_version}'"));
        }
        return Ok((group.to_string(), version.to_string()));
    }
    if api_version.is_empty() {
        return Err(anyhow!("Invalid empty apiVersion"));
    }
    Ok(("".to_string(), api_version.to_string()))
}

async fn build_client(config: &KubernetesSourceConfig) -> Result<Client> {
    let kube_config = if let Some(content) = &config.kubeconfig_content {
        let cfg = Kubeconfig::from_yaml(content)?;
        Config::from_custom_kubeconfig(cfg, &KubeConfigOptions::default()).await?
    } else if let Some(path) = &config.kubeconfig_path {
        let cfg = Kubeconfig::read_from(path)?;
        Config::from_custom_kubeconfig(cfg, &KubeConfigOptions::default()).await?
    } else {
        match config.auth_mode {
            AuthMode::InCluster => Config::incluster_env()
                .map_err(|e| anyhow!("Failed to load in-cluster kube config: {e}"))?,
            AuthMode::Kubeconfig => Config::from_kubeconfig(&KubeConfigOptions::default()).await?,
        }
    };

    Ok(Client::try_from(kube_config)?)
}

fn matches_labels(request: &BootstrapRequest, change: &SourceChange) -> bool {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element, .. } => match element {
            drasi_core::models::Element::Node { metadata, .. } => {
                request.node_labels.is_empty()
                    || metadata.labels.iter().any(|label| {
                        request
                            .node_labels
                            .iter()
                            .any(|allowed| allowed == label.as_ref())
                    })
            }
            drasi_core::models::Element::Relation { metadata, .. } => {
                request.relation_labels.is_empty()
                    || metadata.labels.iter().any(|label| {
                        request
                            .relation_labels
                            .iter()
                            .any(|allowed| allowed == label.as_ref())
                    })
            }
        },
        SourceChange::Delete { metadata } => {
            if request.node_labels.is_empty() && request.relation_labels.is_empty() {
                return true;
            }
            metadata.labels.iter().any(|label| {
                request
                    .node_labels
                    .iter()
                    .any(|allowed| allowed == label.as_ref())
                    || request
                        .relation_labels
                        .iter()
                        .any(|allowed| allowed == label.as_ref())
            })
        }
        SourceChange::Future { .. } => false,
    }
}
