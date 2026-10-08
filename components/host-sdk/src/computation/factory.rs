// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    loader::PluginOwner,
    proxy::{BoundProgress, NativeComponentProxy, SourceBindings},
};
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    transport::{checked_table, take_status},
    wire::{self, CreateRequest, Scope},
};
use drasi_lib::computation::v1::*;
use std::{collections::BTreeMap, sync::Arc};

#[cfg(test)]
#[path = "recovery_tests.rs"]
pub(super) mod recovery_tests;

pub struct NativeFactory {
    metadata: FactoryMetadata,
    descriptor: FactoryDescriptor,
    raw: abi::FactoryHandle,
    _plugin: Arc<PluginOwner>,
    codec: Arc<BinaryEnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
    admission: Option<abi::services::PluginServicesV1>,
    recovery: Option<abi::recovery::PluginRecoveryV1>,
    consumer: Option<(
        drasi_computation_plugin_sdk::ConsumerMode,
        abi::consumer::PluginConsumerV1,
    )>,
}
unsafe impl Send for NativeFactory {}
unsafe impl Sync for NativeFactory {}
impl NativeFactory {
    pub(super) unsafe fn new(
        metadata: FactoryMetadata,
        raw: abi::FactoryHandle,
        plugin: Arc<PluginOwner>,
        codec: Arc<BinaryEnvelopeCodec>,
        schemas: Vec<Arc<Schema>>,
        admission: Option<abi::services::PluginServicesV1>,
        recovery: Option<abi::recovery::PluginRecoveryV1>,
    ) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null() && table.create.is_some() && table.release.is_some(),
            "incomplete native factory table"
        );
        let mut descriptor = metadata.factory_descriptor();
        if admission.is_some() {
            let mut requirement =
                ResourceRequirement::exactly_one::<QosChannel>(ResourceRole::StateStore);
            requirement.minimum = 0;
            descriptor
                .dependencies
                .insert(Arc::from("admission"), requirement);
        }
        if recovery.is_some() {
            let mut requirement = ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                ResourceRole::Checkpoint,
            );
            requirement.minimum = 0;
            descriptor
                .dependencies
                .insert(Arc::from("source_progress"), requirement);
        }
        Ok(Self {
            descriptor,
            metadata,
            raw,
            _plugin: plugin,
            codec,
            schemas,
            admission,
            recovery,
            consumer: None,
        })
    }
    pub fn metadata(&self) -> &FactoryMetadata {
        &self.metadata
    }
    pub(super) fn with_consumer(
        mut self,
        consumer: Option<(
            drasi_computation_plugin_sdk::ConsumerMode,
            abi::consumer::PluginConsumerV1,
        )>,
    ) -> anyhow::Result<Self> {
        if let Some((mode, _)) = consumer {
            mode.validate_factory(&self.metadata)?;
            self.descriptor.dependencies.insert(
                "consumer".into(),
                ResourceRequirement::exactly_one::<super::NativeConsumerResource>(
                    ResourceRole::IndexBackend,
                ),
            );
        }
        self.consumer = consumer;
        Ok(self)
    }
    pub fn consumer_mode(&self) -> Option<drasi_computation_plugin_sdk::ConsumerMode> {
        self.consumer.map(|(mode, _)| mode)
    }
    pub async fn create_consumer(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        options: DeliveryOptions,
    ) -> anyhow::Result<super::NativeConsumerProxy> {
        let (mode, extension) = self
            .consumer
            .ok_or_else(|| anyhow::anyhow!("native factory has no consumer service"))?;
        let component = self.construct_bound(
            CreateRequest {
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: Some(scope.clone()),
            },
            None,
            None,
            true,
        )?;
        let response = unsafe {
            drasi_computation_plugin_sdk::transport::take_reply(extension
                .inspect
                .expect("validated")(
                component.inner.state()
            ))?
        };
        let actual: drasi_computation_plugin_sdk::consumer::FactoryConsumer =
            wire::decode(&response)?;
        anyhow::ensure!(
            actual.version == abi::consumer::VERSION && actual.mode == Some(mode),
            "native consumer changed its negotiated mode"
        );
        super::NativeConsumerProxy::new(
            component,
            extension,
            mode,
            scope,
            provider,
            options,
            self.schemas.clone(),
        )
        .await
    }

    /// Convenience for configuration-only construction outside topology import.
    /// The returned object still has the real graph interfaces/lifecycle hooks.
    pub fn create_component(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct(
            CreateRequest {
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: None,
            },
            None,
            None,
        )
    }

    /// Original, literal construction recipe; the graph retains this for pending
    /// and failed construction. Live configuration is read from the component,
    /// not from a second mutable configuration registry.
    pub fn specification(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        self.metadata.configuration.validate(&configuration)?;
        let configuration = configuration
            .as_object()
            .expect("validated object")
            .iter()
            .map(|(name, value)| {
                (
                    Arc::from(name.as_str()),
                    ConfigurationValue::Literal(value.clone()),
                )
            })
            .collect();
        Ok(ComponentSpecification {
            descriptor: self.metadata.descriptor(id)?,
            role: self.metadata.role,
            completion: self.metadata.completion,
            implementation: self.metadata.implementation.clone(),
            configuration_version: self.metadata.configuration_version,
            configuration,
            dependencies: BTreeMap::new(),
        })
    }

    fn construct(
        &self,
        request: CreateRequest,
        admission: Option<Arc<SourceAdmission>>,
        progress: Option<Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct_bound(request, admission, progress, false)
    }
    fn construct_bound(
        &self,
        request: CreateRequest,
        admission: Option<Arc<SourceAdmission>>,
        progress: Option<Arc<QuerySourceProgress>>,
        consumer: bool,
    ) -> anyhow::Result<NativeComponentProxy> {
        anyhow::ensure!(
            consumer == self.consumer.is_some(),
            "native consumer requires its host-owned delivery binding"
        );
        self.metadata
            .configuration
            .validate(&request.configuration)?;
        let bytes = wire::encode(&request)?;
        let table = unsafe { &*self.raw.vtable };
        let mut component = abi::ComponentHandle::null();
        let binding = admission.as_ref().map(|admission| {
            super::admission::AdmissionBinding::new(admission, self.codec.clone())
        });
        anyhow::ensure!(
            admission.is_none() || progress.is_none(),
            "native source cannot bind admission and a separate replay owner"
        );
        let progress = progress
            .map(|owner| -> anyhow::Result<_> {
                Ok(BoundProgress {
                    binding: super::progress::ProgressBinding::new(&owner),
                    owner,
                    extension: self.recovery.ok_or_else(|| {
                        anyhow::anyhow!("native factory has no source progress service")
                    })?,
                    retention: None,
                })
            })
            .transpose()?;
        unsafe {
            if consumer {
                let (_, extension) = self.consumer.expect("validated");
                take_status(extension.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &mut component,
                ))?;
            } else if let Some(progress) = &progress {
                take_status(progress.extension.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &progress.binding.table(),
                    &mut component,
                ))?;
            } else if let Some(binding) = &binding {
                let services = self.admission.ok_or_else(|| {
                    anyhow::anyhow!("native factory has no source admission service")
                })?;
                take_status(services.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &binding.table(),
                    &mut component,
                ))?;
            } else {
                take_status(table.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &mut component,
                ))?;
            }
        };
        unsafe {
            NativeComponentProxy::new(
                component,
                self.metadata.clone(),
                &request.id,
                self.codec.clone(),
                self.schemas.clone(),
                self._plugin.clone(),
                SourceBindings {
                    admission: admission.zip(binding),
                    progress,
                },
            )
        }
    }
}
impl Drop for NativeFactory {
    fn drop(&mut self) {
        unsafe { (*self.raw.vtable).release.expect("validated")(self.raw.state) };
    }
}

#[async_trait]
impl ComponentFactory for NativeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn record_schemas(&self) -> Vec<Arc<Schema>> {
        self.schemas.clone()
    }
    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.implementation == self.metadata.implementation
                && specification.configuration_version == self.metadata.configuration_version
                && specification.role == self.metadata.role
                && specification.completion == self.metadata.completion,
            "native factory implementation, version, role or completion mismatch"
        );
        anyhow::ensure!(
            specification.dependencies.iter().all(|(name, resources)| {
                resources.len() <= 1
                    && match name.as_ref() {
                        "admission" => self.admission.is_some(),
                        "source_progress" => self.recovery.is_some(),
                        "consumer" => self.consumer.is_some() && resources.len() == 1,
                        _ => false,
                    }
            }),
            "native resource binding requires its negotiated service interface"
        );
        anyhow::ensure!(
            self.consumer.is_none()
                || specification
                    .dependencies
                    .get("consumer")
                    .is_some_and(|ids| ids.len() == 1),
            "native consumer requires one host-owned delivery resource"
        );
        anyhow::ensure!(
            !["admission", "source_progress"].into_iter().all(|name| {
                specification
                    .dependencies
                    .get(name)
                    .is_some_and(|resources| !resources.is_empty())
            }),
            "native source cannot bind admission and a separate replay owner"
        );
        anyhow::ensure!(
            specification.descriptor
                == self
                    .metadata
                    .descriptor(specification.descriptor.id().clone())?,
            "native specification descriptor mismatch"
        );
        if specification
            .configuration
            .values()
            .all(|value| matches!(value, ConfigurationValue::Literal(_)))
        {
            let configuration = serde_json::Value::Object(
                specification
                    .configuration
                    .iter()
                    .map(|(name, value)| {
                        let ConfigurationValue::Literal(value) = value else {
                            unreachable!()
                        };
                        (name.to_string(), value.clone())
                    })
                    .collect(),
            );
            self.metadata.configuration.validate(&configuration)?;
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.validate(&context.specification)
            .map_err(ComponentCreationError::terminal)?;
        if self.consumer.is_some() {
            let resource = context
                .resources::<super::NativeConsumerResource>("consumer")
                .map_err(ComponentCreationError::terminal)?
                .pop()
                .ok_or_else(|| {
                    ComponentCreationError::terminal(anyhow::anyhow!(
                        "missing native consumer delivery resource"
                    ))
                })?;
            let configuration = serde_json::Value::Object(
                context
                    .configuration()
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
            );
            let proxy = self
                .create_consumer(
                    context.component_id,
                    configuration,
                    Scope {
                        instance_id: context.instance_id.to_string(),
                        graph_id: context.graph_id.to_string(),
                        generation: context.generation.0,
                    },
                    resource.provider.clone(),
                    resource.options.clone(),
                )
                .await
                .map_err(ComponentCreationError::terminal)?;
            return Ok(ConstructedComponent::sink(Box::new(proxy)));
        }
        let progress = if context
            .specification
            .dependencies
            .contains_key("source_progress")
        {
            let resources = context
                .resources::<QuerySourceProgressResource>("source_progress")
                .map_err(ComponentCreationError::terminal)?;
            let owner = resources
                .into_iter()
                .next()
                .map(|resource| resource.0.clone());
            if owner.as_ref().is_some_and(|owner| {
                owner.graph_id() != context.graph_id.as_ref()
                    || owner.component_id() == &context.component_id
            }) {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "native progress belongs to another graph or to the source itself"
                )));
            }
            owner
        } else {
            None
        };
        let admission = if context.specification.dependencies.contains_key("admission") {
            let channels = context
                .resources::<QosChannel>("admission")
                .map_err(ComponentCreationError::terminal)?;
            if let Some(channel) = channels.into_iter().next() {
                let admission = SourceAdmission::new(channel, self.metadata.ports[0].id().clone())
                    .await
                    .map_err(ComponentCreationError::terminal)?;
                if admission.component_id() != &context.component_id
                    || admission.identity().graph_id() != context.graph_id.as_ref()
                    || admission.identity().construction_scope() != context.instance_id.as_ref()
                {
                    return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                        "native admission binding belongs to another component or construction scope")));
                }
                Some(admission)
            } else {
                None
            }
        } else {
            None
        };
        let configuration = serde_json::Value::Object(
            context
                .configuration()
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        );
        let proxy = self
            .construct(
                CreateRequest {
                    id: context.component_id,
                    implementation: context.specification.implementation.clone(),
                    configuration_version: context.specification.configuration_version,
                    configuration,
                    scope: Some(Scope {
                        instance_id: context.instance_id.to_string(),
                        graph_id: context.graph_id.to_string(),
                        generation: context.generation.0,
                    }),
                },
                admission,
                progress,
            )
            .map_err(|error| {
                if error
                    .downcast_ref::<super::NativeFailure>()
                    .is_some_and(|failure| failure.retryable)
                {
                    ComponentCreationError::retryable(error)
                } else {
                    ComponentCreationError::terminal(error)
                }
            })?;
        Ok(match self.metadata.role {
            ComponentRole::Source => ConstructedComponent::source(Box::new(proxy)),
            ComponentRole::Transformer => ConstructedComponent::transformer(Box::new(proxy)),
            ComponentRole::Sink => ConstructedComponent::sink(Box::new(proxy)),
            ComponentRole::Service => ConstructedComponent::service(Box::new(proxy)),
            ComponentRole::Query => {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "native dynamic query snapshot/outbox is unsupported"
                )))
            }
        })
    }
}

impl TransactionalTransformerFactory for NativeFactory {
    fn implementation(&self) -> ImplementationIdentity {
        self.metadata.implementation.clone()
    }
    fn configuration_version(&self) -> u32 {
        self.metadata.configuration_version
    }
    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> anyhow::Result<Box<dyn TransactionalTransformer>> {
        anyhow::ensure!(
            self.metadata.capabilities.transactional,
            "native factory did not opt into transaction participation"
        );
        anyhow::ensure!(
            definition.implementation == self.metadata.implementation
                && definition.configuration_version == self.metadata.configuration_version,
            "native transaction implementation/configuration version mismatch"
        );
        Ok(Box::new(
            self.create_component(definition.id.clone(), definition.configuration.clone())?
                .into_transactional()?,
        ))
    }
}
