// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use async_trait::async_trait;
use drasi_computation_bridge_common::{
    config,
    factory::{optional, BridgeFactoryDefinition},
    receiver, transport,
};
use drasi_lib::computation::v1::{
    ComponentCreationError, ComponentFactory, ComponentSpecification, ConstructedComponent,
    ConstructionContext, FactoryDescriptor, PortId, QosChannel, ResourceHandle, ResourceId,
    ResourceSpecification, Schema, SourceAdmission,
};
use std::{collections::BTreeMap, sync::Arc};

pub use component::BridgeSource;
pub use drasi_computation_bridge_common::{
    BridgeConfig, BridgeError, BridgeIngressControl, ErrorCode, IngressCut, IngressDrainStatus,
    Limits, LinkStatus, Namespace, PortableDelivery, Profile, TlsRecipeKind, TlsResource,
    TlsResourceRecipe, TlsRole, Transport, SOURCE_FACTORY,
};

pub fn factory() -> Arc<dyn ComponentFactory> {
    Arc::new(BridgeSourceFactory(BridgeFactoryDefinition::new(true)))
}

struct BridgeSourceFactory(BridgeFactoryDefinition);

#[async_trait]
impl ComponentFactory for BridgeSourceFactory {
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
            let admission = match optional::<QosChannel>(&context, "admission")? {
                Some(channel) => {
                    Some(SourceAdmission::new(channel, PortId::try_new("out")?).await?)
                }
                None => None,
            };
            let mut source =
                BridgeSource::new(config, context.graph_id.to_string(), admission, tls)?;
            if let Some(control) = optional::<BridgeIngressControl>(&context, "control")? {
                source = source.with_ingress_control(control);
            }
            anyhow::Ok(ConstructedComponent::source(Box::new(source)))
        }
        .await
        .map_err(ComponentCreationError::terminal)
    }
}
