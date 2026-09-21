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
    fn id(&self) -> &str;
    fn kind(&self) -> &'static str;
    fn bind_control(&self, _control: ComponentControl) {}
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        None
    }
    async fn initialize(&self, generation: ComponentGeneration) -> anyhow::Result<()>;
    async fn start(&self) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn run(&self) -> anyhow::Result<()>;
    async fn quiesce(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn wait_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn shutdown(&self) -> anyhow::Result<()>;
    async fn deprovision(&self) -> anyhow::Result<()>;
    async fn status(&self) -> crate::ComponentStatus;
}

pub(super) struct RuntimeInstance {
    pub component: Arc<dyn RuntimeComponent>,
    pub token: u64,
    pub installed: AtomicBool,
    pub rejection_owned: AtomicBool,
    remove_data: AtomicBool,
    cleanup_started: AtomicBool,
    shutdown_complete: AtomicBool,
    deprovision_complete: AtomicBool,
    span: tracing::Span,
    record: OnceLock<super::RecordData>,
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
            rejection_owned: AtomicBool::new(false),
            remove_data: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
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
    control: Option<ComponentControl>,
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
    async fn quiesce(&mut self) -> anyhow::Result<()> {
        self.instance
            .component
            .quiesce()
            .instrument(self.instance.span.clone())
            .await
    }

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
    indexes: Arc<crate::indexes::IndexFactory>,
}
impl RuntimeFactory {
    pub(super) fn new(
        services: LegacyPluginServices,
        indexes: Arc<crate::indexes::IndexFactory>,
    ) -> Self {
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
                dependencies: [(
                    Arc::from("instance"),
                    ResourceRequirement::exactly_one::<RuntimeInstance>(ResourceRole::Component),
                )]
                .into_iter()
                .chain(LegacyPluginServices::requirements())
                .chain([(
                    Arc::from("indexes"),
                    ResourceRequirement {
                        minimum: 0,
                        ..ResourceRequirement::exactly_one::<
                            Arc<dyn drasi_core::interface::IndexBackendPlugin>,
                        >(ResourceRole::IndexBackend)
                    },
                )])
                .collect(),
            },
            services,
            indexes,
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
                let selected = match &record.value {
                    super::Value::Query(query) => self
                        .indexes
                        .configured_provider(query.config.storage_backend.as_ref()),
                    _ => None,
                };
                crate::computation::v1::plugin_services::validate_service(
                    spec,
                    resources,
                    "indexes",
                    selected.as_ref().map(|(_, provider)| provider),
                    |provider: &Arc<dyn drasi_core::interface::IndexBackendPlugin>| provider,
                )?;
                if spec.descriptor.id().as_str() != value.component.id() {
                    anyhow::bail!("runtime node ID does not match its component ID");
                }
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
            control: None,
        })))
    }
}
