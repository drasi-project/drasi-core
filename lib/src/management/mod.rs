// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Instance configuration management. Every instance has a configuration store;
//! the default is private in-memory storage. Factories, resource resolution and
//! durable storage are supplied by the embedding host; plugin loading remains external.

mod store;
pub use store::*;
mod authoring;
mod memory;
pub use authoring::{ConfigurationRequest, NewComponentDefinition};
mod transitions;
pub use transitions::{
    validate_recovery_resource_changes, RecoveryRetirementAuthorization, RecoveryTransitionRequired,
};
mod runtime;
pub(crate) use runtime::{AuthoringOperation, Management};
mod ordinary;
mod updates;
pub use ordinary::{
    OrdinaryComponent, OrdinaryComponentDefinition, OrdinaryComponentRecord,
    OrdinaryComponentResolver, OrdinaryComponentUpdateContext, ProcessLocalComponentKind,
};
pub use updates::{AcceptedComponentUpdate, ComponentUpdateCompletion, ComponentUpdateDefinition};

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
pub(crate) use runtime::{compose_definition, equivalent};
use serde::{Deserialize, Serialize};

use crate::computation::v1::{
    ComponentId, ComponentName, DesiredTopology, FactoryRegistry, ImplementationIdentity,
    LifecyclePolicy, PortId, ResourceHandle, ResourceId, ResourceSpecification, StreamId,
};

/// Reconstructible authoring input. Identity assignment occurs inside management
/// and commits with the existing request receipt before any factory executes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentDefinition {
    pub name: ComponentName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ComponentId>,
    pub implementation: ImplementationIdentity,
    pub configuration: serde_json::Value,
    #[serde(default)]
    pub dependencies: BTreeMap<Arc<str>, Vec<ResourceId>>,
    #[serde(default)]
    pub streams: BTreeMap<PortId, StreamId>,
    #[serde(default)]
    pub lifecycle: LifecyclePolicy,
}

/// Declaratively managed components in a DrasiLib instance. Generated query
/// infrastructure and instance-provided services are not reconstruction recipes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredInstance {
    pub version: u32,
    pub topology: DesiredTopology,
    /// Ordinary recipes, including pending and failed declarations. These are
    /// authoritative settings, not snapshots of constructed plugin objects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ordinary: Vec<OrdinaryComponentRecord>,
    /// A revision-bound, persisted authorization to abandon a removed domain's
    /// pending obligations. This neither deletes nor resets its stored data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement: Option<RecoveryRetirementAuthorization>,
}

impl Default for DesiredInstance {
    fn default() -> Self {
        Self {
            version: 2,
            retirement: None,
            ordinary: Vec::new(),
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
            version: 2,
            topology,
            retirement: None,
            ordinary: Vec::new(),
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
/// and bounded by the instance's resource-construction deadline (30 seconds by
/// default); partial acquisitions must be
/// released if the resolver fails or its future is dropped.
#[async_trait]
pub trait ManagementResourceResolver: Send + Sync {
    /// Assess an explicit provider recipe change that replaces live users.
    ///
    /// This is the provider owner's pure configuration assessment, not permission
    /// from its consumers. Both topologies include the provider's prerequisites.
    /// Currently only `Recreate` with `Preserve` is supported: reconciliation
    /// retires old users and ownership before constructing the accepted target.
    /// Borrowed providers cannot be updated through this contract. The separate
    /// `validate_transition` recovery-domain policy still applies.
    fn plan_update(
        &self,
        resource: &ResourceId,
        _previous: &DesiredTopology,
        _desired: &DesiredTopology,
    ) -> anyhow::Result<crate::computation::v1::ComponentUpdatePlan> {
        anyhow::bail!("resource {resource} owner does not support live recipe updates")
    }

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

/// Construction and persistence services. An omitted backend selects the
/// instance's private memory store; it does not disable configuration ownership.
pub struct ManagementOptions {
    pub factories: FactoryRegistry,
    pub resources: Arc<dyn ManagementResourceResolver>,
    pub store: Option<Arc<dyn ConfigurationStore>>,
    pub ordinary: Arc<dyn OrdinaryComponentResolver>,
}

impl Default for ManagementOptions {
    fn default() -> Self {
        Self {
            factories: FactoryRegistry::standard(),
            resources: Arc::new(NoManagementResources),
            store: None,
            ordinary: Arc::new(ordinary::NoOrdinaryComponents),
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
    /// Accept updates and new ID-omitting definitions through one atomic request.
    /// A matching retry returns all original assignments without recreating members.
    pub async fn apply_configuration(
        &self,
        expected_revision: u64,
        request_id: impl Into<String>,
        request: ConfigurationRequest,
    ) -> crate::Result<AcceptanceReceipt> {
        self.management()?
            .apply_configuration(expected_revision, request_id.into(), request)
            .await
            .map_err(Into::into)
    }

    /// Build and accept a complete native recipe batch with scoped UUID allocation.
    /// This callback prepares definitions only: component/resource constructors run
    /// after acceptance. Retries replay the stored allocator transcript.
    pub async fn create_managed_components(
        &self,
        expected_revision: u64,
        request_id: impl Into<String>,
        construct: impl FnOnce(
                &mut crate::computation::v1::ComponentIds,
            ) -> crate::computation::v1::GraphResult<DesiredTopology>
            + Send
            + 'static,
    ) -> crate::Result<AcceptanceReceipt> {
        self.management()?
            .accept(
                expected_revision,
                request_id.into(),
                AuthoringOperation::Native(Box::new(construct)),
            )
            .await
            .map_err(Into::into)
    }

    /// Assign and accept an ordinary recipe before construction. Retry the same
    /// request ID and original input to recover its original UUID/receipt.
    pub async fn create_managed_ordinary_component(
        &self,
        expected_revision: u64,
        request_id: impl Into<String>,
        definition: OrdinaryComponentDefinition,
    ) -> crate::Result<AcceptanceReceipt> {
        self.management()?
            .create_ordinary(expected_revision, request_id.into(), definition)
            .await
            .map_err(Into::into)
    }
    /// Allocate and durably accept one reconstructible component. Reusing the
    /// request ID and identical input returns the same assigned UUID even after
    /// deletion; it never readmits the removed object.
    pub async fn create_managed_component(
        &self,
        expected_revision: u64,
        request_id: impl Into<String>,
        definition: ComponentDefinition,
    ) -> crate::Result<AcceptanceReceipt> {
        self.management()?
            .create_component(expected_revision, request_id.into(), definition)
            .await
            .map_err(Into::into)
    }

    /// Whether this instance has initialized its configuration owner.
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

    /// Supply a factory to existing declarations and retain it for reconciliation.
    pub async fn register_component_factory(
        &self,
        factory: Arc<dyn crate::computation::v1::ComponentFactory>,
    ) -> crate::Result<()> {
        self.state_guard.require_initialized()?;
        if let Some(management) = self.management.get() {
            management.register(factory).await.map_err(Into::into)
        } else {
            self.computation_control()?
                .register_factory(factory)
                .await
                .map_err(|error| crate::DrasiError::Internal(error.into()))
        }
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
        self.management()?
            .restore_snapshot(name.into(), expected_revision, request_id.into())
            .await
            .map_err(Into::into)
    }
}
