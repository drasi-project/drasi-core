// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use async_trait::async_trait;
use drasi_computation_bridge_common::{
    client, config,
    factory::{optional, BridgeFactoryDefinition},
    protocol, state,
};
use drasi_lib::computation::v1::{
    ComponentCreationError, ComponentFactory, ComponentSpecification, ConstructedComponent,
    ConstructionContext, FactoryDescriptor, QueryIndexProviderResource, ResourceHandle, ResourceId,
    ResourceSpecification, Schema,
};
use std::{collections::BTreeMap, sync::Arc};

pub use component::BridgeSink;
pub use drasi_computation_bridge_common::{
    AcceptanceBoundary, BridgeConfig, BridgeControl, BridgeEgressControl, BridgeError,
    EgressStatus, ErrorCode, Limits, LinkStatus, Namespace, PortableEnvelopeCodec, PortableSchema,
    Profile, Receipt, TlsRecipeKind, TlsResource, TlsResourceRecipe, TlsRole, Transport,
    SINK_FACTORY,
};

pub fn factory() -> Arc<dyn ComponentFactory> {
    Arc::new(BridgeSinkFactory(BridgeFactoryDefinition::new(false)))
}

struct BridgeSinkFactory(BridgeFactoryDefinition);

#[async_trait]
impl ComponentFactory for BridgeSinkFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        self.0.descriptor()
    }

    fn record_schemas(&self) -> Vec<Arc<Schema>> {
        self.0.record_schemas()
    }

    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        self.0.validate(specification)
    }

    fn validate_resources(
        &self,
        specification: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.0
            .validate_resources(specification, declarations, resources)
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        async {
            let config = self.0.configuration(&context)?;
            let tls = optional::<TlsResource>(&context, "tls")?;
            let provider = optional::<QueryIndexProviderResource>(&context, "state")?
                .map(|resource| resource.0.clone());
            let mut sink = BridgeSink::new(config, context.graph_id.to_string(), provider, tls)?;
            if let Some(control) = optional::<BridgeEgressControl>(&context, "control")? {
                sink = sink.with_egress_control(control);
            }
            anyhow::Ok(ConstructedComponent::sink(Box::new(sink)))
        }
        .await
        .map_err(ComponentCreationError::terminal)
    }
}
