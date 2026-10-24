// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::computation::v1::{
    ComponentConstruction, ComponentId, ConfigurationValue, DesiredTopology, ResourceId,
};

/// A definite pre-acceptance refusal, not an uncertain configuration commit.
#[derive(Debug, Clone, thiserror::Error)]
#[error("recovery configuration cannot change without verified drain or explicit retirement")]
pub struct RecoveryTransitionRequired {
    resources: BTreeSet<ResourceId>,
    components: BTreeSet<ComponentId>,
}

/// One explicit removal authorization, recorded with the accepted definition and
/// request receipt. Every resource and component in the protected domain must be
/// named and removed. Reusing a prior authorization grants no new permission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRetirementAuthorization {
    pub from_revision: u64,
    pub allow_data_loss: bool,
    pub resources: BTreeSet<ResourceId>,
    pub components: BTreeSet<ComponentId>,
}

impl RecoveryRetirementAuthorization {
    pub(super) fn validate_removed(&self, desired: &DesiredTopology) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.allow_data_loss && !self.resources.is_empty(),
            "retirement requires explicit data-loss authorization and a nonempty resource set"
        );
        anyhow::ensure!(
            desired
                .resources
                .iter()
                .all(|resource| !self.resources.contains(&resource.id))
                && desired
                    .components
                    .iter()
                    .all(|node| !self.components.contains(node.descriptor.id())),
            "loss authorization only permits complete domain removal, not in-place reuse"
        );
        Ok(())
    }

    pub(super) fn validate_transition(
        &self,
        persistent: bool,
        revision: u64,
        previous: &DesiredTopology,
        required: Option<&RecoveryTransitionRequired>,
    ) -> anyhow::Result<()> {
        let refusal = required
            .cloned()
            .unwrap_or_else(|| RecoveryTransitionRequired {
                resources: self.resources.clone(),
                components: self.components.clone(),
            });
        let result = (|| {
            anyhow::ensure!(
                persistent,
                "loss-authorized retirement requires durable configuration storage"
            );
            let required = required.ok_or_else(|| {
                anyhow::anyhow!("retirement authorization requires a protected domain removal")
            })?;
            anyhow::ensure!(
                self.from_revision == revision,
                "retirement authorization belongs to a different configuration revision"
            );
            anyhow::ensure!(self.resources == required.resources && self.components == required.components,
            "retirement must explicitly name every resource and component in the changed recovery domains");
            anyhow::ensure!(
                self.resources
                    .iter()
                    .all(|id| previous.resources.iter().any(|resource| &resource.id == id))
                    && self.components.iter().all(|id| previous
                        .components
                        .iter()
                        .any(|node| node.descriptor.id() == id)),
                "retirement may only abandon the existing managed recovery domain"
            );
            Ok(())
        })();
        result.map_err(|error: anyhow::Error| error.context(refusal))
    }
}

pub(super) struct PreparedRecoveryTransition {
    graph: crate::computation::v1::RecoveryFreeze,
    groups: Vec<drasi_core::computation::TransactionGroupRetirement>,
    journals: Vec<crate::computation::v1::QosRetirement>,
    standalone: Vec<drasi_core::computation::ComputationTransactionRetirement>,
    consumers: Vec<crate::computation::v1::DeliveryRetirement>,
    bootstraps: Vec<Box<dyn crate::computation::v1::BootstrapRetirement>>,
}

impl PreparedRecoveryTransition {
    pub(super) fn retire(self) {
        for bootstrap in self.bootstraps {
            bootstrap.retire();
        }
        for consumer in self.consumers {
            consumer.retire();
        }
        for owner in self.standalone {
            owner.retire();
        }
        for journal in self.journals {
            journal.retire();
        }
        for group in self.groups {
            group.retire();
        }
        self.graph.retire();
    }

    pub(super) fn resume(self) {
        for bootstrap in self.bootstraps {
            bootstrap.resume();
        }
        for consumer in self.consumers {
            consumer.resume();
        }
        for owner in self.standalone {
            owner.resume();
        }
        for journal in self.journals {
            journal.resume();
        }
        for group in self.groups {
            group.resume();
        }
        self.graph.resume();
    }
}

impl RecoveryTransitionRequired {
    pub(super) async fn prepare(
        &self,
        control: &crate::computation::v1::GraphControl,
        previous: &DesiredTopology,
        allow_pending: bool,
    ) -> anyhow::Result<PreparedRecoveryTransition> {
        use crate::computation::v1::{
            ConsumerDeliveryResource, ProcessingRecovery, QosChannel, QueryIndexProviderResource,
            ResourceRole,
        };
        let resources = previous
            .resources
            .iter()
            .filter(|resource| self.resources.contains(&resource.id))
            .map(|resource| resource.id.clone())
            .collect();
        let components = previous
            .components
            .iter()
            .map(|node| node.descriptor.id())
            .filter(|id| self.components.contains(*id))
            .cloned()
            .collect();
        let graph = control
            .freeze_recovery(resources, components, allow_pending)
            .await
            .map_err(|error| anyhow::Error::from(error).context(self.clone()))?;
        let mut prepared = PreparedRecoveryTransition {
            graph,
            groups: Vec::new(),
            journals: Vec::new(),
            standalone: Vec::new(),
            consumers: Vec::new(),
            bootstraps: Vec::new(),
        };
        let result: anyhow::Result<()> = async {
            if allow_pending {
                let actual = control.desired_snapshot().select(crate::computation::v1::GraphSelection::All)?;
                let mut resources = self.resources.clone();
                let mut components = self.components.clone();
                loop {
                    let before = (resources.len(), components.len());
                    expand_domain(&actual, &mut resources, &mut components);
                    if before == (resources.len(), components.len()) {
                        break;
                    }
                }
                anyhow::ensure!(resources == self.resources && components == self.components,
                    "loss authorization cannot omit live users outside the managed recovery domain");
            }
            let mut journals = Vec::new();
            let mut providers = Vec::new();
            for resource in prepared.graph.resources.values() {
                match resource.role() {
                    ResourceRole::IndexBackend => {
                        let provider = if resource.is::<ConsumerDeliveryResource>() {
                            let consumer = resource.get::<ConsumerDeliveryResource>()?;
                            anyhow::ensure!(consumer.provider.transaction_group().is_none(),
                                "consumer retirement requires a standalone delivery owner");
                            consumer.provider.clone()
                        } else {
                            resource.get::<QueryIndexProviderResource>()?.0.clone()
                        };
                        if let Some(group) = provider.transaction_group() {
                            prepared.groups.push(group.freeze_for_retirement()?);
                        } else if !providers
                            .iter()
                            .any(|(actual, _)| std::sync::Arc::ptr_eq(actual, &provider))
                        {
                            providers.push((provider, false));
                        }
                    }
                    ResourceRole::StateStore => {
                        let journal = resource.get::<QosChannel>()?;
                        if journal.is_shared() {
                            journals.push(journal);
                        } else {
                            prepared
                                .journals
                                .push(journal.freeze_for_retirement(allow_pending).await?);
                        }
                    }
                    ResourceRole::Component => {
                        resource.get::<crate::computation::v1::TransactionalTransformerRegistryResource>()?;
                    }
                    ResourceRole::Bootstrap => {
                        let bootstrap = resource.get::<crate::computation::v1::QueryBootstrapResource>()?;
                        prepared.bootstraps.push(bootstrap.0.freeze_for_retirement()?);
                    }
                    ResourceRole::Checkpoint => {
                        let progress = resource.get::<crate::computation::v1::QuerySourceProgressResource>()?;
                        anyhow::ensure!(prepared.graph.contracts.iter().any(|contract|
                            contract.committed_progress.as_ref().is_some_and(|actual|
                                std::sync::Arc::ptr_eq(actual, &progress.0))),
                            "checkpoint resource requires its actual processing owner");
                    }
                    ResourceRole::QueryCatalog => {
                        resource.get::<crate::computation::v1::QueryResultsCatalog>()?;
                    }
                    ResourceRole::SecretStore => {
                        resource.get::<crate::computation::v1::ConfigurationResolverResource>()?;
                    }
                    ResourceRole::Middleware => {
                        if resource.is::<crate::computation::v1::QueryMiddlewareResource>() {
                            resource.get::<crate::computation::v1::QueryMiddlewareResource>()?;
                        } else {
                            resource.get::<crate::computation::v1::MiddlewareRegistryResource>()?;
                        }
                    }
                    _ => anyhow::bail!("resource does not support verified retirement"),
                }
            }
            anyhow::ensure!(
                !prepared.groups.is_empty()
                    || !prepared.journals.is_empty()
                    || !providers.is_empty(),
                "retirement requires an actual storage owner"
            );
            let mut journal_count = 0;
            for group in &prepared.groups {
                journal_count += group.journal_count()?;
                anyhow::ensure!(
                    group.has_processor()?,
                    "recover the shared producer before retirement"
                );
                anyhow::ensure!(
                    allow_pending || !group.has_scheduled_work().await?,
                    "scheduled processing remains"
                );
            }
            let mut producers = 0;
            for contract in &prepared.graph.contracts {
                if let Some(delivery) = &contract.delivery {
                    anyhow::ensure!(contract.output_bindings.is_none() && contract.publication.is_none(),
                        "consumer retirement cannot replace producer output proof");
                    let mut matched = None;
                    for (index, (provider, initialized)) in providers.iter_mut().enumerate() {
                        if let Some(consumer) = delivery.freeze(provider, allow_pending)? {
                            prepared.consumers.push(consumer);
                            *initialized = true;
                            matched = Some(index);
                            break;
                        }
                    }
                    let index = matched.ok_or_else(|| anyhow::anyhow!(
                        "delivery owner is outside the declared storage domain"))?;
                    if let Some(parent) = providers[index].0.provider_dependency().cloned() {
                        let (_, initialized) = providers.iter_mut()
                            .find(|(provider, _)| std::sync::Arc::ptr_eq(provider, &parent))
                            .ok_or_else(|| anyhow::anyhow!(
                                "delivery provider prerequisite is outside the declared storage domain"))?;
                        *initialized = true;
                    }
                } else if let Some(output) = &contract.output_bindings {
                    let mut standalone = false;
                    for (provider, initialized) in &mut providers {
                        if let Some(owner) = output.freeze_standalone(provider)? {
                            prepared.standalone.push(owner);
                            *initialized = true;
                            standalone = true;
                            break;
                        }
                    }
                    output.validate_retirement(allow_pending)?;
                    if !standalone {
                        anyhow::ensure!(
                            !output.has_standalone_owner()?,
                            "standalone processing owner is outside the declared storage domain"
                        );
                        producers += 1;
                    }
                } else {
                    anyhow::ensure!(
                        !matches!(contract.processing, ProcessingRecovery::Atomic(_)),
                        "transactional producer has no initialized retirement evidence"
                    );
                }
            }
            anyhow::ensure!(
                providers.iter().all(|(_, initialized)| *initialized),
                "each standalone provider requires an initialized actual processing owner"
            );
            for owner in &prepared.standalone {
                anyhow::ensure!(
                    allow_pending || !owner.has_scheduled_work().await?,
                    "scheduled processing remains"
                );
            }
            anyhow::ensure!(
                producers == prepared.groups.len(),
                "each shared owner requires an initialized producer"
            );
            anyhow::ensure!(
                journals.len() == journal_count,
                "every live journal must participate in retirement"
            );
            for journal in journals {
                journal.validate_retirement(&prepared.groups, allow_pending).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            prepared.resume();
            return Err(error.context(self.clone()));
        }
        Ok(prepared)
    }
}

/// Conservatively preserve a provider's recovery domains until retirement has
/// been verified. This never infers empty storage from stopped components or a
/// successful shutdown. Resource IDs are declarations, not proof of retirement.
///
/// Hosts identify recipes that can retain processing obligations. Custom
/// resolvers must include their persistent services too. Unrelated domains and
/// lifecycle-only settings can change without modifying a protected domain.
pub fn validate_recovery_resource_changes(
    previous: &DesiredTopology,
    desired: &DesiredTopology,
    protected: impl IntoIterator<Item = ResourceId>,
) -> anyhow::Result<()> {
    let mut remaining: BTreeSet<_> = protected.into_iter().collect();
    let mut changed_resources = BTreeSet::new();
    let mut changed_components = BTreeSet::new();
    while let Some(root) = remaining.pop_first() {
        let mut resources = BTreeSet::from([root]);
        let mut components = BTreeSet::new();
        loop {
            let before = (resources.len(), components.len());
            for topology in [previous, desired] {
                expand_domain(topology, &mut resources, &mut components);
            }
            if before == (resources.len(), components.len()) {
                break;
            }
        }
        remaining.retain(|id| !resources.contains(id));
        if !domain_unchanged(previous, desired, &resources, &components) {
            changed_resources.extend(resources);
            changed_components.extend(components);
        }
    }
    anyhow::ensure!(
        changed_resources.is_empty(),
        RecoveryTransitionRequired {
            resources: changed_resources,
            components: changed_components
        }
    );
    Ok(())
}

fn domain_unchanged(
    previous: &DesiredTopology,
    desired: &DesiredTopology,
    resources: &BTreeSet<ResourceId>,
    components: &BTreeSet<ComponentId>,
) -> bool {
    let unchanged_resources = resources.iter().all(|id| {
        previous
            .resources
            .iter()
            .find(|resource| &resource.id == id)
            == desired.resources.iter().find(|resource| &resource.id == id)
            && previous.resource_configurations.get(id) == desired.resource_configurations.get(id)
            && previous.resource_dependencies.get(id) == desired.resource_dependencies.get(id)
    });
    let unchanged_components = components.iter().all(|id| {
        let left = previous
            .components
            .iter()
            .find(|node| node.descriptor.id() == id);
        let right = desired
            .components
            .iter()
            .find(|node| node.descriptor.id() == id);
        match (left, right) {
            (Some(left), Some(right)) => {
                left.descriptor == right.descriptor
                    && left.role == right.role
                    && left.completion == right.completion
                    && left.streams == right.streams
                    && left.input_merge == right.input_merge
                    && left.construction == right.construction
                    && previous.component_resources.get(id) == desired.component_resources.get(id)
                    && previous.component_plugins.get(id) == desired.component_plugins.get(id)
            }
            (None, None) => true,
            _ => false,
        }
    });
    let edges = |topology: &DesiredTopology| {
        let select = |edges: &[crate::computation::v1::DesiredRelationship]| {
            edges
                .iter()
                .filter(|edge| {
                    components.contains(&edge.definition.from.component)
                        || components.contains(&edge.definition.to.component)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        (
            select(&topology.relationships),
            select(&topology.boundary_relationships),
        )
    };
    let controls = |topology: &DesiredTopology| {
        let select = |links: &[(ComponentId, ComponentId)]| {
            links
                .iter()
                .filter(|(from, to)| components.contains(from) || components.contains(to))
                .cloned()
                .collect::<Vec<_>>()
        };
        (
            select(&topology.control_connections),
            select(&topology.subscriptions),
        )
    };
    let recovery = |topology: &DesiredTopology| {
        topology
            .recovery_requirements
            .iter()
            .filter(|requirement| components.contains(&requirement.consumer))
            .cloned()
            .collect::<Vec<_>>()
    };
    previous.graph_id == desired.graph_id
        && unchanged_resources
        && unchanged_components
        && edges(previous) == edges(desired)
        && controls(previous) == controls(desired)
        && recovery(previous) == recovery(desired)
}

fn expand_domain(
    topology: &DesiredTopology,
    resources: &mut BTreeSet<ResourceId>,
    components: &mut BTreeSet<ComponentId>,
) {
    topology.include_resource_dependents(resources);
    topology.include_resource_dependencies(resources);
    for resource in resources.iter() {
        components.extend(topology.component_resource_users(resource));
    }
    for edge in topology
        .relationships
        .iter()
        .chain(&topology.boundary_relationships)
    {
        let required = edge.pipe.resource_dependencies();
        if components.contains(&edge.definition.from.component)
            || components.contains(&edge.definition.to.component)
            || required.keys().any(|id| resources.contains(id))
        {
            components.insert(edge.definition.from.component.clone());
            components.insert(edge.definition.to.component.clone());
            resources.extend(required.into_keys());
        }
    }
    for (from, to) in topology
        .control_connections
        .iter()
        .chain(&topology.subscriptions)
    {
        if components.contains(from) || components.contains(to) {
            components.insert(from.clone());
            components.insert(to.clone());
        }
    }
    for node in &topology.components {
        let id = node.descriptor.id();
        if !components.contains(id) {
            continue;
        }
        resources.extend(
            topology
                .component_resources
                .get(id)
                .into_iter()
                .flatten()
                .cloned(),
        );
        if let ComponentConstruction::Factory(spec) = &node.construction {
            resources.extend(spec.dependencies.values().flatten().cloned());
            resources.extend(spec.configuration.values().filter_map(|value| match value {
                ConfigurationValue::Reference { resource, .. } => Some(resource.clone()),
                ConfigurationValue::Literal(_) => None,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{computation::v1::*, management::DesiredInstance};
    use std::collections::BTreeMap;

    fn definition() -> DesiredTopology {
        let mut topology = DesiredInstance::default().topology;
        let resource = ResourceId::try_new("storage").unwrap();
        topology.resources.push(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "storage".into(),
        });
        topology
            .resource_configurations
            .insert(resource.clone(), serde_json::json!({"path": "old"}));
        for name in ["owner", "reader", "independent"] {
            let id = ComponentId::try_new(name).unwrap();
            let descriptor = ComponentDescriptor::try_new(id, vec![]).unwrap();
            topology.components.push(DesiredComponent {
                descriptor: descriptor.clone(),
                role: ComponentRole::Service,
                completion: None,
                streams: BTreeMap::new(),
                lifecycle: LifecyclePolicy::default(),
                input_merge: InputMergePolicy::default(),
                construction: ComponentConstruction::Factory(ComponentSpecification {
                    descriptor,
                    role: ComponentRole::Service,
                    completion: None,
                    implementation: ImplementationIdentity::try_new("fixture", "1").unwrap(),
                    configuration_version: 1,
                    configuration: BTreeMap::new(),
                    dependencies: if name == "owner" {
                        BTreeMap::from([("storage".into(), vec![resource.clone()])])
                    } else {
                        BTreeMap::new()
                    },
                }),
            });
        }
        topology.control_connections.push((
            ComponentId::try_new("owner").unwrap(),
            ComponentId::try_new("reader").unwrap(),
        ));
        topology.validate_structure().unwrap();
        topology
    }

    fn validate(previous: &DesiredTopology, next: &DesiredTopology) -> anyhow::Result<()> {
        validate_recovery_resource_changes(
            previous,
            next,
            [ResourceId::try_new("storage").unwrap()],
        )
    }

    #[test]
    fn loss_authorized_retirement_requires_exact_revision_scope_and_complete_removal() {
        let previous = definition();
        let mut next = DesiredInstance::default();
        next.topology
            .components
            .push(previous.components[2].clone());
        let error = validate(&previous, &next.topology).unwrap_err();
        let required = error.downcast_ref::<RecoveryTransitionRequired>().unwrap();
        let authorization = RecoveryRetirementAuthorization {
            from_revision: 7,
            allow_data_loss: true,
            resources: required.resources.clone(),
            components: required.components.clone(),
        };
        authorization.validate_removed(&next.topology).unwrap();
        authorization
            .validate_transition(true, 7, &previous, Some(required))
            .unwrap();
        for (persistent, revision, required) in
            [(false, 7, Some(required)), (true, 8, Some(required)), (true, 7, None)]
        {
            assert!(authorization
                .validate_transition(persistent, revision, &previous, required)
                .unwrap_err()
                .is::<RecoveryTransitionRequired>());
        }
        for mutate in 0..4 {
            let mut invalid = authorization.clone();
            match mutate {
                0 => {
                    invalid.resources.clear();
                }
                1 => {
                    invalid.components.pop_first();
                }
                2 => {
                    invalid
                        .components
                        .insert(ComponentId::try_new("independent").unwrap());
                }
                _ => {
                    invalid
                        .resources
                        .insert(ResourceId::try_new("foreign").unwrap());
                }
            }
            assert!(invalid
                .validate_transition(true, 7, &previous, Some(required))
                .is_err());
        }
        assert!(authorization.validate_removed(&previous).is_err());
        let mut false_permission = authorization.clone();
        false_permission.allow_data_loss = false;
        assert!(false_permission.validate_removed(&next.topology).is_err());
        let legacy = serde_json::to_value(&next).unwrap();
        assert!(legacy.get("retirement").is_none());
        assert_eq!(
            serde_json::from_value::<DesiredInstance>(legacy).unwrap(),
            next
        );
        next.retirement = Some(authorization);
        let normalized = next.normalized().unwrap();
        assert_eq!(
            serde_json::from_value::<DesiredInstance>(serde_json::to_value(&normalized).unwrap())
                .unwrap(),
            normalized
        );
    }

    #[test]
    fn unchanged_recovery_domains_are_not_selected_for_retirement() {
        let mut previous = definition();
        let independent = ResourceId::try_new("independent-storage").unwrap();
        previous.resources.push(ResourceSpecification {
            id: independent.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "independent".into(),
        });
        previous
            .resource_configurations
            .insert(independent.clone(), serde_json::json!({"path": "other"}));
        previous.component_resources.insert(
            ComponentId::try_new("independent").unwrap(),
            BTreeSet::from([independent.clone()]),
        );
        let mut desired = previous.clone();
        let changed = ResourceId::try_new("storage").unwrap();
        desired.resource_configurations.get_mut(&changed).unwrap()["path"] = "replacement".into();
        let error = validate_recovery_resource_changes(
            &previous,
            &desired,
            [changed.clone(), independent.clone()],
        )
        .unwrap_err();
        let selected = error.downcast_ref::<RecoveryTransitionRequired>().unwrap();
        assert_eq!(selected.resources, BTreeSet::from([changed]));
        assert!(!selected
            .components
            .contains(&ComponentId::try_new("independent").unwrap()));
    }

    #[test]
    fn protected_changes_require_retirement_but_unrelated_and_lifecycle_changes_do_not() {
        let previous = definition();
        let mut next = previous.clone();
        next.resource_configurations.values_mut().next().unwrap()["path"] = "new".into();
        assert!(validate(&previous, &next)
            .unwrap_err()
            .is::<RecoveryTransitionRequired>());
        let mut next = previous.clone();
        next.components
            .retain(|node| node.descriptor.id().as_str() != "reader");
        next.control_connections.clear();
        assert!(validate(&previous, &next).is_err());
        let mut next = previous.clone();
        next.subscriptions = std::mem::take(&mut next.control_connections);
        assert!(validate(&previous, &next).is_err());
        let mut next = previous.clone();
        next.components[0].lifecycle.auto_start = !next.components[0].lifecycle.auto_start;
        validate(&previous, &next).unwrap();
        let mut next = previous.clone();
        let ComponentConstruction::Factory(spec) = &mut next.components[2].construction else {
            panic!("factory")
        };
        spec.configuration
            .insert("new".into(), ConfigurationValue::Literal(true.into()));
        validate(&previous, &next).unwrap();
        let mut next = previous.clone();
        let ComponentConstruction::Factory(spec) = &mut next.components[1].construction else {
            panic!("factory")
        };
        spec.configuration
            .insert("new".into(), ConfigurationValue::Literal(true.into()));
        assert!(validate(&previous, &next).is_err());
        validate_recovery_resource_changes(&previous, &next, []).unwrap();
    }

    #[test]
    fn prerequisite_changes_and_new_members_cannot_escape_the_existing_domain() {
        let mut previous = definition();
        let parent = ResourceId::try_new("parent").unwrap();
        previous.resources.push(ResourceSpecification {
            id: parent.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Graph,
            binding: "parent".into(),
        });
        previous
            .resource_configurations
            .insert(parent.clone(), serde_json::json!({"provider":"disk"}));
        previous.resource_dependencies.insert(
            ResourceId::try_new("storage").unwrap(),
            BTreeMap::from([(parent.clone(), ResourceRole::IndexBackend)]),
        );
        let mut next = previous.clone();
        next.resource_configurations.get_mut(&parent).unwrap()["provider"] = "other".into();
        assert!(validate(&previous, &next).is_err());
        let mut next = previous.clone();
        next.control_connections.push((
            ComponentId::try_new("owner").unwrap(),
            ComponentId::try_new("independent").unwrap(),
        ));
        assert!(validate(&previous, &next).is_err());
        let mut next = previous.clone();
        next.resources.clear();
        next.resource_configurations.clear();
        next.resource_dependencies.clear();
        next.components.clear();
        next.control_connections.clear();
        assert!(validate(&previous, &next).is_err());
    }
}
