// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{loader::PluginOwner, proxy::NativeComponentProxy};
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    transport::{checked_table, take_status},
    wire::{self, CreateRequest, Scope},
};
use drasi_lib::computation::v1::*;
use std::{collections::BTreeMap, sync::Arc};

pub struct NativeFactory {
    metadata: FactoryMetadata,
    descriptor: FactoryDescriptor,
    raw: abi::FactoryHandle,
    _plugin: Arc<PluginOwner>,
    codec: Arc<BinaryEnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
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
    ) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null() && table.create.is_some() && table.release.is_some(),
            "incomplete native factory table"
        );
        Ok(Self {
            descriptor: metadata.factory_descriptor(),
            metadata,
            raw,
            _plugin: plugin,
            codec,
            schemas,
        })
    }
    pub fn metadata(&self) -> &FactoryMetadata {
        &self.metadata
    }

    /// Convenience for configuration-only construction outside topology import.
    /// The returned object still has the real graph interfaces/lifecycle hooks.
    pub fn create_component(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct(CreateRequest {
            id,
            implementation: self.metadata.implementation.clone(),
            configuration_version: self.metadata.configuration_version,
            configuration,
            scope: None,
        })
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

    fn construct(&self, request: CreateRequest) -> anyhow::Result<NativeComponentProxy> {
        self.metadata
            .configuration
            .validate(&request.configuration)?;
        let bytes = wire::encode(&request)?;
        let table = unsafe { &*self.raw.vtable };
        let mut component = abi::ComponentHandle::null();
        unsafe {
            take_status(table.create.expect("validated")(
                self.raw.state,
                abi::BorrowedBytes::new(&bytes),
                &mut component,
            ))?
        };
        unsafe {
            NativeComponentProxy::new(
                component,
                self.metadata.clone(),
                &request.id,
                self.codec.clone(),
                self.schemas.clone(),
                self._plugin.clone(),
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
    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.implementation == self.metadata.implementation
                && specification.configuration_version == self.metadata.configuration_version
                && specification.role == self.metadata.role
                && specification.completion == self.metadata.completion,
            "native factory implementation, version, role or completion mismatch"
        );
        anyhow::ensure!(
            specification.dependencies.is_empty(),
            "native ABI 1.0 has no resource injection interface"
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
        let configuration = serde_json::Value::Object(
            context
                .configuration()
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        );
        let proxy = self
            .construct(CreateRequest {
                id: context.component_id,
                implementation: context.specification.implementation.clone(),
                configuration_version: context.specification.configuration_version,
                configuration,
                scope: Some(Scope {
                    instance_id: context.instance_id.to_string(),
                    graph_id: context.graph_id.to_string(),
                    generation: context.generation.0,
                }),
            })
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
