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

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

use super::{
    ComponentCreationError, ComponentDescriptor, ComponentFactory, ComponentId, ComponentRole,
    ComponentSpecification, ComputationComponent, ConfigurationSchema, ConstructedComponent,
    ConstructionContext, ContextEntry, ContextValue, EnvelopeSink, FactoryDescriptor,
    ImplementationIdentity, InputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId,
    QueryChangeCodec, ResourceHandle, ResourceId, ResourceOwnership, ResourceRequirement,
    ResourceRole, ResourceSpecification, SinkCompletion,
};

/// An explicit reference to an already managed legacy Reaction. This resource
/// never owns its initialization, processing loop, stop or deprovision lifecycle.
pub struct LegacyReactionResource(pub Arc<dyn crate::Reaction>);

pub struct LegacyReactionSink {
    descriptor: ComponentDescriptor,
    resource: Arc<LegacyReactionResource>,
}

fn descriptor(id: ComponentId) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![PortDescriptor::new(
            PortId::try_new("in").expect("port"),
            PortDirection::Input,
            QueryChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("reaction input descriptor")
}

impl LegacyReactionSink {
    pub fn borrowed(id: ComponentId, reaction: Arc<dyn crate::Reaction>) -> Self {
        Self {
            descriptor: descriptor(id),
            resource: Arc::new(LegacyReactionResource(reaction)),
        }
    }
}

#[async_trait]
impl ComputationComponent for LegacyReactionSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for LegacyReactionSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, mut input: InputEnvelope) -> anyhow::Result<()> {
        let result = QueryChangeCodec::to_legacy_result(&input.envelope)?;
        self.resource.0.enqueue_query_result(result).await?;
        input.envelope.append_annotation(ContextEntry::try_new(
            self.descriptor.id().clone(),
            "drasi.legacy-reaction.accepted",
            ContextValue::Bool(true),
        )?)?;
        Ok(())
    }
}

pub struct LegacyReactionFactory {
    descriptor: FactoryDescriptor,
}

impl Default for LegacyReactionFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    "drasi/legacy-reaction-adapter",
                    "1",
                )
                .expect("implementation"),
                role: ComponentRole::Sink,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: BTreeMap::from([(
                    Arc::from("reaction"),
                    ResourceRequirement::exactly_one::<LegacyReactionResource>(
                        ResourceRole::LegacyReaction,
                    ),
                )]),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for LegacyReactionFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        if spec.descriptor != descriptor(spec.descriptor.id().clone())
            || spec.completion != Some(SinkCompletion::Accepted)
        {
            anyhow::bail!("legacy reaction adapter accepts typed query rows and declares Accepted, never Handled");
        }
        Ok(())
    }
    fn validate_resources(
        &self,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        _resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        for id in spec.dependencies.get("reaction").into_iter().flatten() {
            if declarations[id].ownership != ResourceOwnership::Borrowed {
                anyhow::bail!("legacy enqueue adapter borrows an externally managed reaction");
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let resource = context
            .resources::<LegacyReactionResource>("reaction")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing reaction")))?;
        Ok(ConstructedComponent::sink(Box::new(LegacyReactionSink {
            descriptor: descriptor(context.component_id),
            resource,
        })))
    }
}
