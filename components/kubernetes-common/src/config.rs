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

/// Specifies a Kubernetes resource to watch, identified by API version and kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSpec {
    /// Kubernetes API version (e.g. `"v1"` for core resources, `"apps/v1"` for grouped resources).
    pub api_version: String,
    /// Kubernetes resource kind (e.g. `"Pod"`, `"Deployment"`, `"ConfigMap"`).
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
    /// Resume from a specific point in time. The value is a Unix timestamp in seconds.
    Timestamp(i64),
}

pub fn default_annotation_excludes() -> Vec<String> {
    vec![
        "kubectl.kubernetes.io/last-applied-configuration".to_string(),
        "control-plane.alpha.kubernetes.io/leader".to_string(),
    ]
}

/// Configuration for the Kubernetes source plugin.
///
/// This struct is shared between the source and bootstrap plugins via the
/// `drasi-kubernetes-common` crate.
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

/// Returns true if the given kind is supported by the Kubernetes source plugin.
///
/// The initial implementation supports core and apps API group resources only.
/// CRD support is planned for a future iteration.
pub fn is_supported_kind(kind: &str) -> bool {
    matches!(
        kind,
        "Pod" | "Deployment" | "ReplicaSet" | "Node" | "Service" | "ConfigMap" | "Namespace"
    )
}

/// Returns true if the given kind is cluster-scoped (not namespaced).
/// This list is exhaustive for all currently supported kinds returned by `is_supported_kind`.
pub fn is_cluster_scoped_kind(kind: &str) -> bool {
    matches!(kind, "Node" | "Namespace")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_kinds_accepted() {
        for kind in &[
            "Pod",
            "Deployment",
            "ReplicaSet",
            "Node",
            "Service",
            "ConfigMap",
            "Namespace",
        ] {
            assert!(is_supported_kind(kind), "{kind} should be supported");
        }
    }

    #[test]
    fn unsupported_kinds_rejected() {
        for kind in &["DaemonSet", "StatefulSet", "Job", "CronJob", "Ingress", ""] {
            assert!(!is_supported_kind(kind), "{kind} should not be supported");
        }
    }

    #[test]
    fn cluster_scoped_kinds() {
        assert!(is_cluster_scoped_kind("Node"));
        assert!(is_cluster_scoped_kind("Namespace"));
    }

    #[test]
    fn namespaced_kinds_are_not_cluster_scoped() {
        for kind in &["Pod", "Deployment", "ReplicaSet", "Service", "ConfigMap"] {
            assert!(
                !is_cluster_scoped_kind(kind),
                "{kind} should not be cluster-scoped"
            );
        }
    }

    #[test]
    fn default_annotation_excludes_non_empty() {
        let defaults = default_annotation_excludes();
        assert!(!defaults.is_empty());
        assert!(defaults.contains(&"kubectl.kubernetes.io/last-applied-configuration".to_string()));
    }

    #[test]
    fn validate_empty_resources_fails() {
        let config = KubernetesSourceConfig::default();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_valid_config() {
        let config = KubernetesSourceConfig {
            resources: vec![ResourceSpec {
                api_version: "v1".to_string(),
                kind: "Pod".to_string(),
            }],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_unsupported_kind_fails() {
        let config = KubernetesSourceConfig {
            resources: vec![ResourceSpec {
                api_version: "apps/v1".to_string(),
                kind: "DaemonSet".to_string(),
            }],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_mutual_exclusive_kubeconfig_options() {
        let config = KubernetesSourceConfig {
            resources: vec![ResourceSpec {
                api_version: "v1".to_string(),
                kind: "Pod".to_string(),
            }],
            kubeconfig_path: Some("/path".to_string()),
            kubeconfig_content: Some("content".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn serde_exclude_annotations_absent_uses_defaults() {
        let json = serde_json::json!({
            "resources": [{"apiVersion": "v1", "kind": "Pod"}]
        });
        let config: KubernetesSourceConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.exclude_annotations, default_annotation_excludes());
    }

    #[test]
    fn serde_exclude_annotations_explicit_empty_preserves_empty() {
        let json = serde_json::json!({
            "resources": [{"apiVersion": "v1", "kind": "Pod"}],
            "excludeAnnotations": []
        });
        let config: KubernetesSourceConfig = serde_json::from_value(json).unwrap();
        assert!(config.exclude_annotations.is_empty());
    }

    #[test]
    fn serde_exclude_annotations_custom_list() {
        let json = serde_json::json!({
            "resources": [{"apiVersion": "v1", "kind": "Pod"}],
            "excludeAnnotations": ["my.custom/annotation"]
        });
        let config: KubernetesSourceConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.exclude_annotations, vec!["my.custom/annotation"]);
    }
}
