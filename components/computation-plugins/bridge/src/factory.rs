// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{any::TypeId, collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use drasi_lib::computation::v1::{
    ComponentCreationError, ComponentFactory, ComponentRole, ComponentSpecification,
    ConfigurationField, ConfigurationSchema, ConfigurationType, ConfigurationValue,
    ConstructedComponent, ConstructionContext, FactoryDescriptor, ImplementationIdentity, PortId,
    QosChannel, QueryIndexProviderResource, ResourceHandle, ResourceId, ResourceRequirement,
    ResourceRole, ResourceSpecification, Schema, SourceAdmission,
};

use crate::{
    BridgeConfig, BridgeEgressControl, BridgeIngressControl, BridgeSink, BridgeSource,
    PortableSchema, Profile, TlsResource, SINK_FACTORY, SOURCE_FACTORY,
};

pub(crate) struct BridgeFactory {
    descriptor: FactoryDescriptor,
    source: bool,
}

impl BridgeFactory {
    pub(crate) fn new(source: bool) -> Self {
        use ConfigurationType::{Object, String};
        let fields = [
            ("link_id", String, true),
            ("sender_instance", String, true),
            ("receiver_instance", String, true),
            ("sender_endpoint", String, true),
            ("receiver_endpoint", String, true),
            ("stream", String, true),
            ("peer_stream", String, true),
            ("schema", String, true),
            ("profile", String, true),
            ("failure_scope", String, false),
            ("namespace", Object, true),
            ("transport", Object, true),
            ("limits", Object, false),
        ]
        .into_iter()
        .map(|(name, value_type, required)| {
            (
                Arc::from(name),
                ConfigurationField {
                    value_type,
                    required,
                    secret: false,
                },
            )
        })
        .collect();
        let optional = |role, concrete_type| ResourceRequirement {
            role,
            minimum: 0,
            maximum: Some(1),
            concrete_type: Some(concrete_type),
        };
        let mut dependencies = BTreeMap::from([(
            Arc::from("tls"),
            optional(ResourceRole::Identity, TypeId::of::<TlsResource>()),
        )]);
        if source {
            dependencies.insert(
                Arc::from("control"),
                optional(
                    ResourceRole::Component,
                    TypeId::of::<BridgeIngressControl>(),
                ),
            );
            dependencies.insert(
                Arc::from("admission"),
                optional(ResourceRole::StateStore, TypeId::of::<QosChannel>()),
            );
        } else {
            dependencies.insert(
                Arc::from("control"),
                optional(ResourceRole::Component, TypeId::of::<BridgeEgressControl>()),
            );
            dependencies.insert(
                Arc::from("state"),
                optional(
                    ResourceRole::IndexBackend,
                    TypeId::of::<QueryIndexProviderResource>(),
                ),
            );
        }
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    if source { SOURCE_FACTORY } else { SINK_FACTORY },
                    "1",
                )
                .expect("constant bridge factory identity"),
                role: if source {
                    ComponentRole::Source
                } else {
                    ComponentRole::Sink
                },
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields,
                    allow_additional: false,
                },
                dependencies,
            },
            source,
        }
    }

    fn validate_config(
        &self,
        specification: &ComponentSpecification,
        config: &BridgeConfig,
    ) -> anyhow::Result<()> {
        config.validate()?;
        anyhow::ensure!(
            specification.descriptor == config.descriptor(self.descriptor.role)?
                && specification.completion == (!self.source).then(|| config.completion()),
            "bridge descriptor, ports, schema, requirements or completion differ from configuration"
        );
        let present = |slot: &str| {
            specification
                .dependencies
                .get(slot)
                .is_some_and(|ids| ids.len() == 1)
        };
        anyhow::ensure!(
            (config.profile == Profile::Durable) == present(if self.source { "admission" } else { "state" }),
            "durable bridge needs its actual admission/state resource; volatile forbids that dependency"
        );
        anyhow::ensure!(
            config.transport.is_tls() == present("tls"),
            "TLS transport requires exactly one TLS identity resource"
        );
        Ok(())
    }

    fn literal_config(
        &self,
        specification: &ComponentSpecification,
    ) -> anyhow::Result<Option<BridgeConfig>> {
        let mut object = serde_json::Map::new();
        for (name, value) in &specification.configuration {
            match value {
                ConfigurationValue::Literal(value) => {
                    object.insert(name.to_string(), value.clone());
                }
                ConfigurationValue::Reference { .. } => return Ok(None),
            }
        }
        Ok(Some(serde_json::from_value(serde_json::Value::Object(
            object,
        ))?))
    }
}

#[async_trait]
impl ComponentFactory for BridgeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn record_schemas(&self) -> Vec<Arc<Schema>> {
        [
            PortableSchema::GraphChange,
            PortableSchema::QueryChange,
            PortableSchema::SourceTransaction,
        ]
        .into_iter()
        .map(PortableSchema::schema)
        .collect()
    }

    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.role == self.descriptor.role
                && specification.implementation == self.descriptor.implementation
                && specification.configuration_version == 1,
            "bridge factory role or configuration version mismatch"
        );
        if let Some(config) = self.literal_config(specification)? {
            self.validate_config(specification, &config)?;
        }
        Ok(())
    }

    fn validate_resources(
        &self,
        specification: &ComponentSpecification,
        _declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(specification)?;
        if let Some(config) = self.literal_config(specification)? {
            if self.source && config.profile == Profile::Durable {
                let id = &specification.dependencies["admission"][0];
                if let Some(resource) = resources.get(id) {
                    let channel = resource.get::<QosChannel>()?;
                    anyhow::ensure!(
                        channel.definition().stream.as_str() == config.stream,
                        "bridge source journal has another stream"
                    );
                    channel.durability().require(config.failure_scope)?;
                }
            }
        }
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        async {
            let object = context
                .configuration()
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            let config: BridgeConfig = serde_json::from_value(serde_json::Value::Object(object))?;
            self.validate_config(&context.specification, &config)?;
            anyhow::ensure!(
                context.instance_id.as_ref()
                    == if self.source {
                        &config.receiver_instance
                    } else {
                        &config.sender_instance
                    },
                "bridge endpoint belongs to a different instance"
            );
            let tls = optional::<TlsResource>(&context, "tls")?;
            if self.source {
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
                Ok(ConstructedComponent::source(Box::new(source)))
            } else {
                let provider = optional::<QueryIndexProviderResource>(&context, "state")?
                    .map(|resource| resource.0.clone());
                let mut sink =
                    BridgeSink::new(config, context.graph_id.to_string(), provider, tls)?;
                if let Some(control) = optional::<BridgeEgressControl>(&context, "control")? {
                    sink = sink.with_egress_control(control);
                }
                Ok(ConstructedComponent::sink(Box::new(sink)))
            }
        }
        .await
        .map_err(ComponentCreationError::terminal)
    }
}

fn optional<T: std::any::Any + Send + Sync>(
    context: &ConstructionContext,
    name: &str,
) -> anyhow::Result<Option<Arc<T>>> {
    if context
        .specification
        .dependencies
        .get(name)
        .is_none_or(Vec::is_empty)
    {
        Ok(None)
    } else {
        Ok(context.resources::<T>(name)?.into_iter().next())
    }
}
