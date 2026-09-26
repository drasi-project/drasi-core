// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional desired-state management. Factories, resource resolution and durable
//! storage are supplied by the embedding host; plugin loading remains external.

mod store;
pub use store::*;
mod runtime;
pub(crate) use runtime::Management;

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::computation::v1::{
    DesiredTopology, FactoryRegistry, ResourceHandle, ResourceId, ResourceSpecification,
};

/// Complete user-managed graph set for one DrasiLib instance. Omitted graphs
/// are removed. Generated ordinary-query implementation graphs are not recipes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredInstance {
    pub version: u32,
    pub graphs: Vec<DesiredGraph>,
}

impl Default for DesiredInstance {
    fn default() -> Self {
        Self {
            version: 1,
            graphs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredGraph {
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    pub topology: DesiredTopology,
}

fn default_auto_start() -> bool {
    true
}

impl From<DesiredTopology> for DesiredInstance {
    fn from(topology: DesiredTopology) -> Self {
        Self {
            version: 1,
            graphs: vec![DesiredGraph {
                auto_start: true,
                topology,
            }],
        }
    }
}

/// Resolve a persisted provider recipe. Implementations retain loading, secret
/// and backend-specific policy outside drasi-lib. Successful resources transfer
/// the declared cleanup ownership to the graph.
#[async_trait]
pub trait ManagementResourceResolver: Send + Sync {
    async fn resolve(
        &self,
        instance_id: &str,
        graph_id: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> anyhow::Result<ResourceHandle>;
}

#[derive(Default)]
pub struct NoManagementResources;

#[async_trait]
impl ManagementResourceResolver for NoManagementResources {
    async fn resolve(
        &self,
        _: &str,
        _: &str,
        specification: &ResourceSpecification,
        _: &serde_json::Value,
    ) -> anyhow::Result<ResourceHandle> {
        anyhow::bail!("no resolver supplied for resource {}", specification.id)
    }
}

/// Optional construction and persistence services. Persistence is off by default.
pub struct ManagementOptions {
    pub factories: FactoryRegistry,
    pub resources: Arc<dyn ManagementResourceResolver>,
    pub store: Option<Arc<dyn ConfigurationStore>>,
}

impl Default for ManagementOptions {
    fn default() -> Self {
        Self {
            factories: FactoryRegistry::standard(),
            resources: Arc::new(NoManagementResources),
            store: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphManagementStatus {
    pub graph_id: String,
    pub applied: bool,
    pub converged: bool,
    pub error: Option<String>,
    pub resource_errors: BTreeMap<ResourceId, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementStatus {
    pub revision: u64,
    pub persistent: bool,
    pub reconciling: bool,
    pub error: Option<String>,
    pub graphs: Vec<GraphManagementStatus>,
}

impl ManagementStatus {
    pub fn converged(&self) -> bool {
        !self.reconciling
            && self.error.is_none()
            && self
                .graphs
                .iter()
                .all(|graph| graph.converged && graph.error.is_none())
    }
}

impl crate::DrasiLib {
    fn management(&self) -> crate::Result<&Management> {
        self.management.get().ok_or_else(|| crate::DrasiError::invalid_state(
                "management is not configured; supply ManagementOptions or a component factory registry",
            ))
    }

    /// Durably accept a complete managed graph set before construction. The
    /// receipt means accepted, not ready. Call reconcile_desired_state to wait
    /// for a pass and inspect individual creation/activation failures.
    pub async fn apply_desired_state(
        &self,
        expected_revision: u64,
        request_id: impl Into<String>,
        desired: DesiredInstance,
    ) -> crate::Result<AcceptanceReceipt> {
        self.management()?
            .apply(expected_revision, request_id.into(), desired)
            .await
            .map_err(Into::into)
    }

    /// Privileged: definitions may contain literal credentials or secret refs.
    pub fn desired_configuration(&self) -> crate::Result<CommittedConfiguration> {
        Ok(self.management()?.configuration())
    }

    pub async fn management_status(&self) -> crate::Result<ManagementStatus> {
        self.management()?.status().await.map_err(Into::into)
    }

    pub async fn reconcile_desired_state(&self) -> crate::Result<ManagementStatus> {
        self.management()?.reconcile().await.map_err(Into::into)
    }

    pub async fn register_component_factory(
        &self,
        factory: Arc<dyn crate::computation::v1::ComponentFactory>,
    ) -> crate::Result<()> {
        self.management()?
            .register(factory)
            .await
            .map_err(Into::into)
    }

    pub async fn configuration_receipt(
        &self,
        request_id: impl Into<String>,
    ) -> crate::Result<Option<AcceptanceReceipt>> {
        self.management()?
            .receipt(request_id.into())
            .await
            .map_err(Into::into)
    }

    /// Privileged, immutable committed configuration snapshot, including nodes
    /// that have never constructed successfully. Does not snapshot runtime data.
    pub async fn snapshot_desired_configuration(
        &self,
        name: impl Into<String>,
    ) -> crate::Result<CommittedConfiguration> {
        self.management()?
            .snapshot(name.into())
            .await
            .map_err(Into::into)
    }

    pub async fn load_configuration_snapshot(
        &self,
        name: impl Into<String>,
    ) -> crate::Result<Option<CommittedConfiguration>> {
        self.management()?
            .load_snapshot(name.into())
            .await
            .map_err(Into::into)
    }

    pub async fn restore_configuration_snapshot(
        &self,
        name: impl Into<String>,
        expected_revision: u64,
        request_id: impl Into<String>,
    ) -> crate::Result<AcceptanceReceipt> {
        let snapshot = self
            .load_configuration_snapshot(name)
            .await?
            .ok_or_else(|| {
                crate::DrasiError::invalid_config("configuration snapshot does not exist")
            })?;
        self.apply_desired_state(expected_revision, request_id, snapshot.desired)
            .await
    }
}
