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

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSpec {
    pub api_version: String,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    #[default]
    Kubeconfig,
    InCluster,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "type", content = "timestamp", rename_all = "snake_case")]
pub enum StartFrom {
    #[default]
    Now,
    Beginning,
    Timestamp(i64),
}

pub fn default_annotation_excludes() -> Vec<String> {
    vec![
        "kubectl.kubernetes.io/last-applied-configuration".to_string(),
        "control-plane.alpha.kubernetes.io/leader".to_string(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KubernetesSourceConfig {
    pub resources: Vec<ResourceSpec>,
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub label_selector: Option<String>,
    #[serde(default)]
    pub field_selector: Option<String>,
    #[serde(default)]
    pub auth_mode: AuthMode,
    #[serde(default)]
    pub kubeconfig_path: Option<String>,
    #[serde(default)]
    pub kubeconfig_content: Option<String>,
    #[serde(default)]
    pub include_owner_relations: bool,
    #[serde(default)]
    pub start_from: StartFrom,
    #[serde(default = "default_annotation_excludes")]
    pub exclude_annotations: Vec<String>,
}

impl Default for KubernetesSourceConfig {
    fn default() -> Self {
        Self {
            resources: Vec::new(),
            namespaces: Vec::new(),
            label_selector: None,
            field_selector: None,
            auth_mode: AuthMode::default(),
            kubeconfig_path: None,
            kubeconfig_content: None,
            include_owner_relations: false,
            start_from: StartFrom::default(),
            exclude_annotations: default_annotation_excludes(),
        }
    }
}

impl KubernetesSourceConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.resources.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: resources cannot be empty. \
                 Configure at least one resource kind (e.g. Pod, Deployment, ConfigMap)"
            ));
        }

        for res in &self.resources {
            if res.api_version.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "Validation error: api_version cannot be empty for kind '{}'",
                    res.kind
                ));
            }

            if res.kind.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "Validation error: resource kind cannot be empty for api_version '{}'",
                    res.api_version
                ));
            }

            if !is_supported_kind(&res.kind) {
                return Err(anyhow::anyhow!(
                    "Validation error: unsupported kind '{}'. \
                     Initial Kubernetes source implementation supports only core+apps kinds: \
                     Pod, Deployment, ReplicaSet, Node, Service, ConfigMap, Namespace",
                    res.kind
                ));
            }
        }

        if self.kubeconfig_content.is_some() && self.kubeconfig_path.is_some() {
            return Err(anyhow::anyhow!(
                "Validation error: kubeconfig_content and kubeconfig_path are mutually exclusive"
            ));
        }

        Ok(())
    }
}

pub fn is_supported_kind(kind: &str) -> bool {
    matches!(
        kind,
        "Pod" | "Deployment" | "ReplicaSet" | "Node" | "Service" | "ConfigMap" | "Namespace"
    )
}

pub fn is_cluster_scoped_kind(kind: &str) -> bool {
    matches!(kind, "Node" | "Namespace")
}
