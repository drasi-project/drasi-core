// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use drasi_lib::{
    computation::v1::{ComponentConstruction, ComponentId, ConfigurationValue},
    management::DesiredInstance,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_HIERARCHY_DEPTH: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlugin {
    /// An exact library path, not a directory to discover or execute.
    pub path: PathBuf,
    /// Optional content pin, checked before loading any executable metadata.
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationStorage {
    /// An external, owner-only file containing exactly 32 raw key bytes.
    pub key_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IndexStorage {
    Rocksdb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeBudget {
    pub worker_threads: usize,
    pub blocking_threads: usize,
    pub max_owned_children: usize,
    pub max_control_connections: usize,
    pub max_control_frame_bytes: usize,
    pub max_control_operations: usize,
    pub diagnostic_bytes: usize,
    pub index_memory_bytes: usize,
    pub startup_timeout_ms: u64,
    pub operation_timeout_ms: u64,
    pub peer_timeout_ms: u64,
    pub health_interval_ms: u64,
    /// A requested hard sandbox is rejected rather than confused with scheduling.
    pub require_hard_resource_isolation: bool,
    pub require_kernel_parent_death: bool,
}

impl Default for RuntimeBudget {
    fn default() -> Self {
        Self {
            worker_threads: 2,
            blocking_threads: 4,
            max_owned_children: 8,
            max_control_connections: 8,
            max_control_frame_bytes: MAX_MANIFEST_BYTES + 64 * 1024,
            max_control_operations: 128,
            diagnostic_bytes: 4096,
            index_memory_bytes: 64 * 1024 * 1024,
            startup_timeout_ms: 30_000,
            operation_timeout_ms: 30_000,
            peer_timeout_ms: 500,
            health_interval_ms: 250,
            require_hard_resource_isolation: false,
            require_kernel_parent_death: false,
        }
    }
}

impl RuntimeBudget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.worker_threads),
            "worker_threads must be 1..64"
        );
        ensure!(
            (1..=64).contains(&self.blocking_threads),
            "blocking_threads must be 1..64"
        );
        ensure!(
            self.max_owned_children <= 32,
            "at most 32 local children are supported"
        );
        ensure!(
            (1..=64).contains(&self.max_control_connections),
            "invalid control connection budget"
        );
        ensure!(
            (4096..=2 * MAX_MANIFEST_BYTES).contains(&self.max_control_frame_bytes),
            "invalid control frame budget"
        );
        ensure!(
            (8..=1024).contains(&self.max_control_operations),
            "invalid operation budget"
        );
        ensure!(
            (256..=65536).contains(&self.diagnostic_bytes),
            "invalid diagnostic budget"
        );
        ensure!(
            (1024 * 1024..=1024 * 1024 * 1024).contains(&self.index_memory_bytes),
            "invalid shared RocksDB memory budget"
        );
        for timeout in [self.startup_timeout_ms, self.operation_timeout_ms] {
            ensure!(
                (100..=300_000).contains(&timeout),
                "lifecycle timeout must be 100..300000 ms"
            );
        }
        ensure!(
            (20..=5000).contains(&self.peer_timeout_ms),
            "invalid peer timeout"
        );
        ensure!(
            (20..=5000).contains(&self.health_interval_ms),
            "invalid health interval"
        );
        ensure!(
            !self.require_hard_resource_isolation,
            "hard CPU/memory/security isolation is unsupported; runtime budgets are not an OS sandbox"
        );
        ensure!(
            !self.require_kernel_parent_death,
            "kernel-enforced parent-death fencing is unsupported; the Unix watchdog requires OS scheduling"
        );
        Ok(())
    }
}

/// Host-level cuts, not a special DrasiLib shutdown mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StopStage {
    Quiesce { components: Vec<ComponentId> },
    StopChild { component: ComponentId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub version: u32,
    pub instance_id: String,
    /// Stable, private directory exclusively locked by this instance process.
    pub state_dir: PathBuf,
    #[serde(default)]
    pub plugins: Vec<NativePlugin>,
    #[serde(default)]
    pub desired: DesiredInstance,
    #[serde(default)]
    pub runtime: RuntimeBudget,
    #[serde(default)]
    pub configuration_store: Option<ConfigurationStorage>,
    #[serde(default)]
    pub indexes: BTreeMap<String, IndexStorage>,
    /// Required for graphs containing inter-instance data endpoints.
    #[serde(default)]
    pub stop_stages: Vec<StopStage>,
}

impl HostConfig {
    pub fn empty(instance_id: impl Into<String>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            version: 1,
            instance_id: instance_id.into(),
            state_dir: state_dir.into(),
            plugins: Vec::new(),
            desired: DesiredInstance::default(),
            runtime: RuntimeBudget::default(),
            configuration_store: None,
            indexes: BTreeMap::new(),
            stop_stages: Vec::new(),
        }
    }

    /// Pure structural validation. In particular this never loads a child's
    /// library, asks for its factory metadata, or runs its record validators.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_MANIFEST_BYTES,
            "manifest exceeds byte budget"
        );
        let mut instances = BTreeSet::new();
        let mut paths = BTreeSet::new();
        self.validate_tree(0, &mut instances, &mut paths)
    }

    fn validate_tree(
        &self,
        depth: usize,
        instances: &mut BTreeSet<String>,
        paths: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        ensure!(
            depth <= MAX_HIERARCHY_DEPTH,
            "instance hierarchy is too deep"
        );
        ensure!(self.version == 1, "unsupported host manifest version");
        validate_name(&self.instance_id)?;
        validate_path(&self.state_dir)?;
        ensure!(
            instances.insert(self.instance_id.clone()),
            "instance identity is shared across owned hosts"
        );
        ensure!(
            paths.insert(self.state_dir.clone()),
            "state directory is shared across owned hosts"
        );
        self.runtime.validate()?;
        ensure!(self.plugins.len() <= 64, "plugin allowlist exceeds budget");
        let mut plugins = BTreeSet::new();
        for plugin in &self.plugins {
            validate_path(&plugin.path)?;
            ensure!(
                plugins.insert(plugin.path.clone()),
                "duplicate plugin allowlist path"
            );
            if let Some(hash) = &plugin.sha256 {
                ensure!(
                    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid plugin SHA-256"
                );
            }
        }
        if let Some(store) = &self.configuration_store {
            validate_path(&store.key_file)?;
        }
        ensure!(self.indexes.len() <= 32, "index-provider budget exceeded");
        for name in self.indexes.keys() {
            validate_name(name)?;
        }
        let children = self.child_configs()?;
        ensure!(
            children.len() <= self.runtime.max_owned_children,
            "owned-child budget exceeded"
        );
        for child in children.values() {
            child.validate_tree(depth + 1, instances, paths)?;
        }
        ensure!(self.stop_stages.len() <= 256, "stop-stage budget exceeded");
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }

    pub fn child_configs(&self) -> Result<BTreeMap<String, HostConfig>> {
        let mut children = BTreeMap::new();
        for component in &self.desired.topology.components {
            let ComponentConstruction::Factory(spec) = &component.construction else {
                continue;
            };
            if spec.implementation.name.as_ref() == crate::SUPERVISOR_IMPLEMENTATION {
                let ConfigurationValue::Literal(manifest) = spec
                    .configuration
                    .get("manifest")
                    .context("child supervisor requires a literal manifest")?
                else {
                    anyhow::bail!("child manifests cannot be resolved by parent plugin code");
                };
                children.insert(
                    component.descriptor.id().to_string(),
                    serde_json::from_value(manifest.clone())?,
                );
            }
        }
        Ok(children)
    }

    pub(crate) fn validate_stop_plan(&self, desired: &DesiredInstance) -> Result<()> {
        let mut configured = self.clone();
        configured.desired = desired.clone();
        let children = configured.child_configs()?;
        let bridge_components: Vec<_> = desired
            .topology
            .components
            .iter()
            .filter_map(|component| {
                let ComponentConstruction::Factory(spec) = &component.construction else {
                    return None;
                };
                matches!(
                    spec.implementation.name.as_ref(),
                    "drasi.bridge/source" | "drasi.bridge/sink"
                )
                .then_some((component, spec))
            })
            .collect();
        if bridge_components.is_empty() {
            return Ok(());
        }
        ensure!(!self.stop_stages.is_empty(), "bridge composition requires an explicit quiesce/child-stop plan; endpoints remain open");
        let mut cuts = BTreeMap::new();
        let mut stops = BTreeMap::new();
        for (stage, value) in self.stop_stages.iter().enumerate() {
            match value {
                StopStage::Quiesce { components } => {
                    for id in components {
                        ensure!(
                            cuts.insert(id.to_string(), stage).is_none(),
                            "duplicate quiesce cut"
                        );
                    }
                }
                StopStage::StopChild { component } => {
                    ensure!(
                        children.contains_key(component.as_str()),
                        "stop plan refers to an unowned child"
                    );
                    ensure!(
                        stops.insert(component.to_string(), stage).is_none(),
                        "duplicate child-stop stage"
                    );
                }
            }
        }
        ensure!(
            children.keys().all(|id| stops.contains_key(id)),
            "stop plan does not cover every owned child"
        );
        for (component, spec) in bridge_components {
            let source = spec.implementation.name.as_ref() == "drasi.bridge/source";
            let peer_key = if source {
                "sender_instance"
            } else {
                "receiver_instance"
            };
            let peer = match spec.configuration.get(peer_key) {
                Some(ConfigurationValue::Literal(value)) => value.as_str(),
                _ => None,
            }
            .context("composition stop requires a literal bridge peer identity")?;
            let child = children.iter().find(|(_, child)| child.instance_id == peer);
            if source {
                let cut = cuts
                    .get(component.descriptor.id().as_str())
                    .context("every bridge ingress needs an explicit local drain/quiesce stage")?;
                if let Some((id, _)) = child {
                    ensure!(
                        *cut > stops[id],
                        "return ingress must remain live until its child stops"
                    );
                }
            } else {
                let cut = cuts
                    .get(component.descriptor.id().as_str())
                    .context("every bridge egress needs an explicit local quiesce cut")?;
                let mut upstream = vec![component.descriptor.id().clone()];
                let mut seen = BTreeSet::new();
                while let Some(consumer) = upstream.pop() {
                    if !seen.insert(consumer.clone()) {
                        continue;
                    }
                    for producer in desired
                        .topology
                        .relationships
                        .iter()
                        .filter(|edge| edge.definition.to.component == consumer)
                        .map(|edge| &edge.definition.from.component)
                        .chain(
                            desired
                                .topology
                                .subscriptions
                                .iter()
                                .filter(|(_, target)| *target == consumer)
                                .map(|(source, _)| source),
                        )
                    {
                        ensure!(cuts.get(producer.as_str()).is_some_and(|stage| stage <= cut),
                            "bridge egress cut must include its complete local upstream path before the receiver stops");
                        upstream.push(producer.clone());
                    }
                }
                if let Some((id, _)) = child {
                    ensure!(
                        *cut < stops[id],
                        "drain child-bound egress before stopping the receiver"
                    );
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
            && value != "."
            && value != "..",
        "identity must contain 1..128 ASCII letters, digits, '.', '_' or '-'"
    );
    Ok(())
}

pub(crate) fn validate_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "host paths must be absolute and stable");
    ensure!(
        path.components()
            .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)),
        "host paths must be normalized"
    );
    Ok(())
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn manifest_identity_does_not_include_process_generation() {
        let config = HostConfig::empty("stable", PathBuf::from("/stable/instance"));
        config.validate().unwrap();
        let decoded: HostConfig =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(
            config.fingerprint().unwrap(),
            decoded.fingerprint().unwrap()
        );
        assert!(HostConfig::empty("../escape", "/stable")
            .validate()
            .is_err());
    }

    #[test]
    fn impossible_hard_resource_guarantees_are_rejected() {
        let mut config = HostConfig::empty("host", "/stable");
        config.runtime.require_hard_resource_isolation = true;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
    }
}
