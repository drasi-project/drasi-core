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
        Arc, OnceLock,
    },
};
use tracing::Instrument;

#[async_trait]
pub(super) trait RuntimeComponent: Send + Sync {
    fn id(&self) -> &ComponentId;
    fn kind(&self) -> &'static str;
    fn metadata(&self) -> Option<std::collections::HashMap<String, String>> {
        None
    }
    fn configuration_available(&self) -> bool {
        true
    }
    async fn construct(&self, _generation: ComponentGeneration) -> anyhow::Result<()> {
        Ok(())
    }
    fn dependency_waits(
        &self,
        _desired: &GraphSnapshot,
        _observed: &ObservedGraph,
    ) -> Vec<DependencyWait> {
        Vec::new()
    }
    fn bind_control(&self, _control: ComponentControl) {}
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        None
    }
    async fn initialize(&self, generation: ComponentGeneration) -> anyhow::Result<()>;
    async fn start(&self) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn run(&self) -> anyhow::Result<()>;
    async fn pause_processing(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn apply_update(
        &self,
        _context: crate::management::OrdinaryComponentUpdateContext,
    ) -> anyhow::Result<()> {
        anyhow::bail!("runtime component does not support retained-object updates")
    }
    async fn wait_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn shutdown(&self) -> anyhow::Result<()>;
    async fn delete_owned_resources(&self) -> anyhow::Result<()>;
}

pub(crate) struct RuntimeInstance {
    pub(super) component: Arc<dyn RuntimeComponent>,
    pub(super) token: u64,
    remove_data: AtomicBool,
    cleanup_started: AtomicBool,
    shutdown_complete: AtomicBool,
    stop_complete: AtomicBool,
    deprovision_complete: AtomicBool,
    span: tracing::Span,
    record: OnceLock<super::RecordData>,
}
impl RuntimeInstance {
    pub(super) fn cleanup_started(&self) -> bool {
        self.cleanup_started.load(Ordering::Acquire)
    }

    pub(crate) fn dependency_waits(
        &self,
        desired: &GraphSnapshot,
        observed: &ObservedGraph,
    ) -> Vec<DependencyWait> {
        self.component.dependency_waits(desired, observed)
    }

    pub(super) fn new(component: Arc<dyn RuntimeComponent>, token: u64, instance: &str) -> Self {
        let span = tracing::info_span!(
            "native_runtime_component",
            instance_id = instance,
            component_id = %component.id(),
            component_type = component.kind(),
        );
        Self {
            component,
            token,
            remove_data: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
            stop_complete: AtomicBool::new(false),
            deprovision_complete: AtomicBool::new(false),
            span,
            record: OnceLock::new(),
        }
    }
    pub(super) fn remove_data(&self) {
        self.remove_data.store(true, Ordering::Release);
    }
    pub(super) fn bind_record(&self, record: super::RecordData) -> anyhow::Result<()> {
        self.record
            .set(record)
            .map_err(|_| anyhow::anyhow!("component already has a graph record"))
    }
    pub(super) fn record(self: &Arc<Self>) -> anyhow::Result<super::Record> {
        self.record
            .get()
            .map(|record| record.record(self.clone()))
            .ok_or_else(|| anyhow::anyhow!("component instance has no graph record"))
    }
}
#[async_trait]
impl ResourceCleanup for RuntimeInstance {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.cleanup_started.store(true, Ordering::Release);
        if !self.stop_complete.load(Ordering::Acquire) {
            self.component.stop().instrument(self.span.clone()).await?;
            self.stop_complete.store(true, Ordering::Release);
        }
        if self.remove_data.load(Ordering::Acquire)
            && !self.deprovision_complete.load(Ordering::Acquire)
        {
            self.component
                .delete_owned_resources()
                .instrument(self.span.clone())
                .await?;
            self.deprovision_complete.store(true, Ordering::Release);
        }
        if !self.shutdown_complete.load(Ordering::Acquire) {
            self.component
                .shutdown()
                .instrument(self.span.clone())
                .await?;
            self.shutdown_complete.store(true, Ordering::Release);
        }
        Ok(())
    }
}

struct Service {
    descriptor: ComponentDescriptor,
    // The graph-owned resource retains cleanup ownership, including after failed initialization.
    instance: Arc<RuntimeInstance>,
    control: Option<ComponentControl>,
    generation: ComponentGeneration,
}
#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.instance.component.bind_control(control.clone());
        self.control = Some(control);
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.instance.component.control_handler()
    }
    fn requires_readiness_confirmation(&self) -> bool {
        true
    }
    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.instance
            .component
            .construct(self.generation)
            .instrument(self.instance.span.clone())
            .await?;
        self.instance
            .component
            .initialize(self.generation)
            .instrument(self.instance.span.clone())
            .await
    }
    async fn pause_processing(&mut self) -> anyhow::Result<()> {
        self.instance
            .component
            .pause_processing()
            .instrument(self.instance.span.clone())
            .await
    }
    async fn resume_processing(&mut self) -> anyhow::Result<()> {
        self.instance
            .component
            .resume_processing()
            .instrument(self.instance.span.clone())
            .await
    }
    async fn apply_update(&mut self, context: ComponentUpdateContext) -> anyhow::Result<()> {
        let current = match context.current.configuration.get("definition") {
            Some(ConfigurationValue::Literal(value)) => serde_json::from_value(value.clone())?,
            _ => anyhow::bail!("runtime update has no registered current definition"),
        };
        let proposed = serde_json::from_value(
            context
                .proposed
                .configuration()
                .get("definition")
                .ok_or_else(|| anyhow::anyhow!("runtime update has no proposed definition"))?
                .clone(),
        )?;
        self.instance
            .component
            .apply_update(crate::management::OrdinaryComponentUpdateContext {
                operation_id: context.operation_id,
                instance_id: context.proposed.instance_id.clone(),
                plan: context.plan,
                current: Arc::new(current),
                proposed: Arc::new(proposed),
            })
            .instrument(self.instance.span.clone())
            .await
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
        let run = self
            .instance
            .component
            .run()
            .instrument(self.instance.span.clone());
        let ready = self
            .instance
            .component
            .wait_ready()
            .instrument(self.instance.span.clone());
        tokio::pin!(run, ready);
        tokio::select! {
            biased;
            result = &mut ready => {
                result?;
                self.control.as_ref().ok_or_else(|| anyhow::anyhow!("component control is not bound"))?.ready()?;
                run.await
            }
            result = &mut run => {
                result?;
                anyhow::bail!("component processing completed before readiness was confirmed")
            }
        }
    }
}

pub(super) struct RuntimeFactory {
    descriptor: FactoryDescriptor,
    services: LegacyPluginServices,
}
impl RuntimeFactory {
    pub(super) fn new(services: LegacyPluginServices) -> Self {
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
                                    constraint: None,
                                    value_type: ConfigurationType::String,
                                    required: true,
                                    secret: false,
                                },
                            )
                        })
                        .collect(),
                    allow_additional: true,
                },
                dependencies: [(
                    Arc::from("instance"),
                    ResourceRequirement::exactly_one::<RuntimeInstance>(ResourceRole::Component),
                )]
                .into_iter()
                .chain([(
                    Arc::from("source_host"),
                    ResourceRequirement {
                        minimum: 0,
                        ..ResourceRequirement::exactly_one::<SourcePluginHost>(
                            ResourceRole::LegacySource,
                        )
                    },
                )])
                .chain(LegacyPluginServices::requirements())
                .collect(),
            },
            services,
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
        self.services.validate_bindings(spec, resources)?;
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
                let record = value.record()?;
                if matches!(
                    &record.value,
                    super::Value::Source(_) | super::Value::Reaction(_)
                ) {
                    if let (Some(id), Some(version)) = (
                        record.metadata.get("pluginId"),
                        record.metadata.get("pluginVersion"),
                    ) {
                        let identity = PluginIdentity {
                            id: id.as_str().into(),
                            version: version.as_str().into(),
                        };
                        spec.descriptor
                            .clone()
                            .with_plugin_identity(identity.clone())?;
                        if spec.descriptor.plugin_identity() != Some(&identity) {
                            anyhow::bail!(
                                "runtime plugin provenance does not match its supplied metadata"
                            );
                        }
                    }
                }
                if spec.descriptor.id() != value.component.id() {
                    anyhow::bail!("runtime node ID does not match its component ID");
                }
                for (key, expected) in [
                    ("id", serde_json::json!(value.component.id())),
                    ("kind", serde_json::json!(value.component.kind())),
                ] {
                    if spec.configuration.get(key) != Some(&ConfigurationValue::Literal(expected)) {
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
        Ok(ConstructedComponent::service(Box::new(Service {
            descriptor: context.specification.descriptor.clone(),
            instance,
            control: None,
            generation: context.generation,
        })))
    }
}
