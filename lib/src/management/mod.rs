// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional desired-state management. Factories, resource resolution and durable
//! storage are supplied by the embedding host; plugin loading remains external.

mod store;
pub use store::*;
mod transitions;
pub use transitions::{
    validate_recovery_resource_changes, RecoveryRetirementAuthorization, RecoveryTransitionRequired,
};
mod runtime;
pub(crate) use runtime::Management;

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
pub(crate) use runtime::{compose_definition, equivalent};
use serde::{Deserialize, Serialize};

use crate::computation::v1::{
    DesiredTopology, FactoryRegistry, ResourceHandle, ResourceId, ResourceSpecification,
};

/// Declaratively managed components in a DrasiLib instance. QueryGraph
/// internals and instance-provided services are not reconstruction recipes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredInstance {
    pub version: u32,
    pub topology: DesiredTopology,
    /// A revision-bound, persisted authorization to abandon a removed domain's
    /// pending obligations. This neither deletes nor resets its stored data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement: Option<RecoveryRetirementAuthorization>,
}

impl Default for DesiredInstance {
    fn default() -> Self {
        Self {
            version: 1,
            retirement: None,
            topology: crate::computation::v1::ComputationGraph::empty(
                crate::computation::components::INSTANCE_GRAPH_ID,
            )
            .expect("instance graph identity")
            .snapshot()
            .select(crate::computation::v1::GraphSelection::All)
            .expect("empty component definition"),
        }
        .normalized()
        .expect("empty managed component definition")
    }
}

impl From<DesiredTopology> for DesiredInstance {
    fn from(mut topology: DesiredTopology) -> Self {
        topology.graph_id = crate::computation::components::INSTANCE_GRAPH_ID.into();
        Self {
            version: 1,
            topology,
            retirement: None,
        }
    }
}

impl DesiredInstance {
    /// Validate reconstructibility and canonicalize declaration ordering without
    /// invoking factories or providers. Hosts seeding a configuration store must
    /// use the same form as ordinary desired-state acceptance.
    pub fn normalized(self) -> anyhow::Result<Self> {
        runtime::normalize(self)
    }
}

/// Resolve a persisted provider recipe. Implementations retain loading, secret
/// and backend-specific policy outside drasi-lib. Successful resources transfer
/// the declared cleanup ownership to the graph. Replacements are constructed
/// only after the graph releases the prior owner. Resolution is cancellation-safe
/// and bounded by a 30-second deadline; partial acquisitions must be
/// released if the resolver fails or its future is dropped.
#[async_trait]
pub trait ManagementResourceResolver: Send + Sync {
    /// Pure pre-acceptance validation, including when old resources could not be
    /// reconstructed. Resolvers offering persistent processing services must
    /// protect their recovery domains; wrappers must forward this policy.
    /// Never assume that a missing live handle means storage is empty.
    fn validate_transition(
        &self,
        _previous: &DesiredTopology,
        _desired: &DesiredTopology,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn resolve(
        &self,
        instance_id: &str,
        graph_id: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> anyhow::Result<ResourceHandle>;

    async fn resolve_with_dependencies(
        &self,
        instance_id: &str,
        graph_id: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<ResourceHandle> {
        anyhow::ensure!(
            dependencies.is_empty(),
            "resource resolver does not support dependencies"
        );
        self.resolve(instance_id, graph_id, specification, configuration)
            .await
    }
}

/// Bind a recipe without acquiring its storage. The graph constructs it after
/// dependencies exist and previous owners have completed cleanup.
pub fn resource_constructor(
    resolver: Arc<dyn ManagementResourceResolver>,
    instance_id: impl Into<String>,
    graph_id: impl Into<String>,
    specification: ResourceSpecification,
    configuration: serde_json::Value,
) -> Arc<dyn crate::computation::v1::ResourceConstructor> {
    Arc::new(runtime::ManagedResource {
        resolver,
        instance_id: instance_id.into(),
        graph_id: graph_id.into(),
        specification,
        configuration,
    })
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
pub struct ManagementStatus {
    pub revision: u64,
    pub persistent: bool,
    pub reconciling: bool,
    pub error: Option<String>,
    pub applied: bool,
    pub converged: bool,
    pub resource_errors: BTreeMap<ResourceId, String>,
}

impl ManagementStatus {
    pub fn converged(&self) -> bool {
        !self.reconciling
            && self.error.is_none()
            && self.applied
            && self.converged
            && self.resource_errors.is_empty()
    }
}

impl crate::DrasiLib {
    /// Whether the instance accepts revisioned desired definitions.
    pub fn has_managed_configuration(&self) -> bool {
        self.management.get().is_some()
    }

    /// Whether accepted configuration is backed by an external durable store.
    pub fn configuration_is_persistent(&self) -> bool {
        self.management.get().is_some_and(Management::persistent)
    }

    fn management(&self) -> crate::Result<&Management> {
        self.management.get().ok_or_else(|| crate::DrasiError::invalid_state(
                "management is not configured; supply ManagementOptions or a component factory registry",
            ))
    }

    /// Durably accept managed component definitions before construction. The
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
    /// Returns an error while an ambiguous commit cannot be confirmed by the
    /// store; reconciliation reloads authoritative state before applying it.
    pub fn desired_configuration(&self) -> crate::Result<CommittedConfiguration> {
        self.management()?.configuration().map_err(Into::into)
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

    /// Generate an ID for a new configuration operation. Reuse the same ID for
    /// retries. Opt-in expiry stores may expire a full previous batch here.
    pub async fn new_configuration_request_id(&self) -> crate::Result<String> {
        self.management()?
            .new_request_id()
            .await
            .map_err(Into::into)
    }

    pub async fn list_configuration_snapshots(
        &self,
        after: Option<String>,
        limit: std::num::NonZeroUsize,
    ) -> crate::Result<Vec<ConfigurationSnapshotSummary>> {
        self.management()?
            .list_snapshots(after, limit)
            .await
            .map_err(Into::into)
    }

    pub async fn delete_configuration_snapshot(
        &self,
        name: impl Into<String>,
        expected_revision: u64,
    ) -> crate::Result<bool> {
        self.management()?
            .delete_snapshot(name.into(), expected_revision)
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
