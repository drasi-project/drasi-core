// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use super::v1::*;

pub(crate) const INSTANCE_GRAPH_ID: &str = "__drasi_lib_runtime__";

/// Component declarations and supplied bindings for one instance-level addition.
pub struct ComponentBatch {
    pub definition: DesiredTopology,
    pub bindings: TopologyBindings,
}

impl ComponentBatch {
    pub fn builder() -> ComponentBatchBuilder {
        ComponentBatchBuilder {
            inner: ComputationGraph::builder(INSTANCE_GRAPH_ID).for_additions(),
        }
    }

    pub fn auto_start(mut self, enabled: bool) -> Self {
        for component in &mut self.definition.components {
            component.lifecycle.auto_start = enabled;
        }
        self
    }
}

/// Assemble components, resources and connections without starting their work.
pub struct ComponentBatchBuilder {
    inner: ComputationGraphBuilder,
}

impl ComponentBatchBuilder {
    pub fn source(mut self, source: Box<dyn EnvelopeSource>) -> Self {
        self.inner = self.inner.source(source);
        self
    }

    pub fn transformer(mut self, transformer: Box<dyn Transformer>) -> Self {
        self.inner = self.inner.transformer(transformer);
        self
    }

    pub fn query(mut self, query: Box<dyn Transformer>) -> Self {
        self.inner = self.inner.query(query);
        self
    }

    pub fn sink(mut self, sink: Box<dyn EnvelopeSink>) -> Self {
        self.inner = self.inner.sink(sink);
        self
    }

    pub fn service(mut self, service: Box<dyn ComputationService>) -> Self {
        self.inner = self.inner.service(service);
        self
    }

    pub fn component(
        mut self,
        specification: ComponentSpecification,
        factory: Arc<dyn ComponentFactory>,
    ) -> Self {
        self.inner = self.inner.component(specification, factory);
        self
    }

    pub fn declare_resource(mut self, specification: ResourceSpecification) -> GraphResult<Self> {
        self.inner = self.inner.declare_resource(specification)?;
        Ok(self)
    }

    pub fn resource_configuration(
        mut self,
        resource: ResourceId,
        configuration: serde_json::Value,
    ) -> GraphResult<Self> {
        self.inner = self.inner.resource_configuration(resource, configuration)?;
        Ok(self)
    }

    pub fn provide_resource(
        mut self,
        id: ResourceId,
        resource: ResourceHandle,
    ) -> GraphResult<Self> {
        self.inner = self.inner.provide_resource(id, resource)?;
        Ok(self)
    }

    pub fn connect(mut self, edge: EdgeDefinition, provider: Box<dyn PipeProvider>) -> Self {
        self.inner = self.inner.connect(edge, provider);
        self
    }

    pub fn bind_stream(mut self, output: Endpoint, stream: StreamId) -> Self {
        self.inner = self.inner.bind_stream(output, stream);
        self
    }

    pub fn lifecycle_policy(mut self, component: ComponentId, policy: LifecyclePolicy) -> Self {
        self.inner = self.inner.lifecycle_policy(component, policy);
        self
    }

    pub fn relationship_policy(mut self, edge: EdgeDefinition, policy: RelationshipPolicy) -> Self {
        self.inner = self.inner.relationship_policy(edge, policy);
        self
    }

    pub fn input_merge(mut self, component: ComponentId, policy: InputMergePolicy) -> Self {
        self.inner = self.inner.input_merge(component, policy);
        self
    }

    pub fn require_downstream_ready(mut self, component: ComponentId) -> Self {
        self.inner = self.inner.require_downstream_ready(component);
        self
    }

    pub fn requirements(mut self, requirements: PipeRequirements) -> Self {
        self.inner = self.inner.requirements(requirements);
        self
    }

    pub fn cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.cleanup_timeout(timeout);
        self
    }

    pub fn build(self) -> GraphResult<ComponentBatch> {
        self.inner.build_components()
    }
}

pub(crate) fn append_definition(
    mut target: DesiredTopology,
    addition: &DesiredTopology,
    existing: &BTreeMap<ResourceId, ResourceHandle>,
    supplied: &BTreeMap<ResourceId, ResourceHandle>,
) -> anyhow::Result<DesiredTopology> {
    for component in &addition.components {
        anyhow::ensure!(
            !target
                .components
                .iter()
                .any(|current| current.descriptor.id() == component.descriptor.id()),
            "component {} already exists",
            component.descriptor.id()
        );
    }
    for resource in &addition.resources {
        if let Some(current) = target
            .resources
            .iter()
            .find(|current| current.id == resource.id)
        {
            anyhow::ensure!(
                current == resource
                    && target.resource_configurations.get(&resource.id)
                        == addition.resource_configurations.get(&resource.id)
                    && existing
                        .get(&resource.id)
                        .zip(supplied.get(&resource.id))
                        .is_some_and(|(left, right)| left.same_shared_instance(right)),
                "resource {} already exists with a different binding",
                resource.id
            );
        } else {
            target.resources.push(resource.clone());
        }
    }
    target
        .components
        .extend(addition.components.iter().cloned());
    target
        .relationships
        .extend(addition.relationships.iter().cloned());
    target
        .boundary_relationships
        .extend(addition.boundary_relationships.iter().cloned());
    target
        .resource_configurations
        .extend(addition.resource_configurations.clone());
    target
        .control_connections
        .extend(addition.control_connections.iter().cloned());
    target
        .subscriptions
        .extend(addition.subscriptions.iter().cloned());
    target
        .readiness_required
        .extend(addition.readiness_required.iter().cloned());
    target
        .component_resources
        .extend(addition.component_resources.clone());
    target
        .component_plugins
        .extend(addition.component_plugins.clone());
    target.requirements = PipeRequirements::new(
        target
            .requirements
            .required()
            .iter()
            .chain(addition.requirements.required())
            .copied(),
    );
    target.validate_structure()?;
    Ok(target)
}
