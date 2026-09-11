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

use crate::computation::v1::*;
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tracing::Instrument;

#[async_trait]
pub(super) trait RuntimeComponent: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> &'static str;
    async fn initialize(&self, generation: ComponentGeneration) -> anyhow::Result<()>;
    async fn start(&self) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn run(&self) -> anyhow::Result<()>;
    async fn shutdown(&self) -> anyhow::Result<()>;
    async fn deprovision(&self) -> anyhow::Result<()>;
    async fn status(&self) -> crate::ComponentStatus;
}

pub(super) struct RuntimeInstance {
    pub component: Arc<dyn RuntimeComponent>,
    pub token: u64,
    pub installed: AtomicBool,
    remove_data: AtomicBool,
    cleanup_started: AtomicBool,
    shutdown_complete: AtomicBool,
    deprovision_complete: AtomicBool,
    span: tracing::Span,
}
impl RuntimeInstance {
    pub(super) fn new(component: Arc<dyn RuntimeComponent>, token: u64, instance: &str) -> Self {
        let span = tracing::info_span!(
            "native_runtime_component",
            instance_id = instance,
            component_id = component.id(),
            component_type = component.kind(),
        );
        Self {
            component,
            token,
            installed: AtomicBool::new(false),
            remove_data: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
            deprovision_complete: AtomicBool::new(false),
            span,
        }
    }
    pub(super) fn remove_data(&self) {
        self.remove_data.store(true, Ordering::Release);
    }
    pub(super) fn cancel_unstarted_removal(&self) {
        if !self.cleanup_started.load(Ordering::Acquire) {
            self.remove_data.store(false, Ordering::Release);
        }
    }
}
#[async_trait]
impl ResourceCleanup for RuntimeInstance {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.cleanup_started.store(true, Ordering::Release);
        if !self.shutdown_complete.load(Ordering::Acquire) {
            self.component
                .shutdown()
                .instrument(self.span.clone())
                .await?;
            self.shutdown_complete.store(true, Ordering::Release);
        }
        if self.remove_data.load(Ordering::Acquire)
            && !self.deprovision_complete.load(Ordering::Acquire)
        {
            self.component
                .deprovision()
                .instrument(self.span.clone())
                .await?;
            self.deprovision_complete.store(true, Ordering::Release);
        }
        Ok(())
    }
}

struct Service {
    descriptor: ComponentDescriptor,
    instance: Arc<RuntimeInstance>,
}
#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.instance
            .component
            .start()
            .instrument(self.instance.span.clone())
            .await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.instance
            .component
            .stop()
            .instrument(self.instance.span.clone())
            .await
    }
}
#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.instance
            .component
            .run()
            .instrument(self.instance.span.clone())
            .await
    }
}

pub(super) struct RuntimeFactory {
    descriptor: FactoryDescriptor,
}
impl Default for RuntimeFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/lib-runtime-component", "1")
                    .expect("implementation"),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: ["id", "kind"]
                        .into_iter()
                        .map(|key| {
                            (
                                Arc::from(key),
                                ConfigurationField {
                                    value_type: ConfigurationType::String,
                                    required: true,
                                    secret: false,
                                },
                            )
                        })
                        .collect(),
                    allow_additional: true,
                },
                dependencies: BTreeMap::from([(
                    Arc::from("instance"),
                    ResourceRequirement::exactly_one::<RuntimeInstance>(ResourceRole::Component),
                )]),
            },
        }
    }
}
#[async_trait]
impl ComponentFactory for RuntimeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        if !spec.descriptor.ports().is_empty() {
            anyhow::bail!("runtime services have no data ports");
        }
        Ok(())
    }
    fn validate_resources(
        &self,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        if let Some(id) = spec
            .dependencies
            .get("instance")
            .and_then(|ids| ids.first())
        {
            if declarations[id].ownership != ResourceOwnership::Graph {
                anyhow::bail!("runtime component must have graph cleanup ownership");
            }
            if let Some(resource) = resources.get(id) {
                let value = resource.get::<RuntimeInstance>()?;
                for (key, expected) in
                    [("id", value.component.id()), ("kind", value.component.kind())]
                {
                    if spec.configuration.get(key)
                        != Some(&ConfigurationValue::Literal(expected.into()))
                    {
                        anyhow::bail!("runtime instance identity does not match its specification");
                    }
                    if spec.configuration.get("record_token")
                        != Some(&ConfigurationValue::Literal(value.token.into()))
                    {
                        anyhow::bail!("runtime record does not match its construction token");
                    }
                }
                if !resource.has_cleanup() {
                    anyhow::bail!("runtime instance has no cleanup owner");
                }
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let instance = context
            .resources::<RuntimeInstance>("instance")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing runtime instance"))
            })?;
        instance
            .component
            .initialize(context.generation)
            .instrument(instance.span.clone())
            .await
            .map_err(ComponentCreationError::retryable)?;
        Ok(ConstructedComponent::service(Box::new(Service {
            descriptor: context.specification.descriptor.clone(),
            instance,
        })))
    }
}
