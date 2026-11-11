// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentDescriptor, ComponentRole, DeliveryHandler, DeliveryItem,
    GraphChangeCodec, ImplementationIdentity, InputBinding, InputEnvelope, OutputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId, Schema, SinkCompletion,
};
use drasi_computation_plugin_sdk as sdk;
use std::{io::Write, path::PathBuf, sync::Arc};

pub struct Factory;

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/input-bindings", "1")
                .expect("fixture identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: sdk::ConfigSchema::from_fields([(
                "observations",
                sdk::ConfigType::String,
                true,
            )]),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(name, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(name).expect("fixture port"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: sdk::Capabilities {
                input_binding_updates: true,
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        Ok(
            sdk::Component::Transformer(Box::new(Transformer::new(request, self.metadata())?))
                .into(),
        )
    }
}

pub struct TransactionalFactory;

impl sdk::Factory for TransactionalFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        let mut metadata = Factory.metadata();
        metadata.implementation =
            ImplementationIdentity::try_new("fixture/input-binding-transactional", "1")
                .expect("fixture identity");
        metadata.capabilities.transactional = true;
        metadata
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        Ok(
            sdk::Component::Transactional(Box::new(Transformer::new(request, self.metadata())?))
                .into(),
        )
    }
}

struct Transformer {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    observations: PathBuf,
    bindings: Option<Vec<InputBinding>>,
}

impl Transformer {
    fn new(request: &sdk::CreateRequest, metadata: sdk::FactoryMetadata) -> anyhow::Result<Self> {
        let observations = request
            .configuration
            .get("observations")
            .and_then(serde_json::Value::as_str)
            .context("missing observations file")?;
        Ok(Self {
            descriptor: metadata.descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            observations: PathBuf::from(observations),
            bindings: None,
        })
    }
}

#[async_trait]
impl sdk::NativeComponent for Transformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }

    fn supports_input_binding_updates(&self) -> bool {
        true
    }

    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        self.bindings = Some(bindings.to_vec());
        Ok(())
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.bindings.is_some(),
            "inputs were not bound before start"
        );
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeTransformer for Transformer {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let observation = serde_json::json!({
            "stream": input.envelope.system().stream().as_str(),
            "bindings": self.bindings.as_ref().context("missing input bindings")?,
        });
        let mut line = serde_json::to_vec(&observation)?;
        line.push(b'\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.observations)?
            .write_all(&line)?;
        Ok(Vec::new())
    }
}

#[async_trait]
impl sdk::TransactionalComponent for Transformer {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }

    fn transaction_output_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }

    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        _: &sdk::NativeTransactionContext<'_>,
    ) -> anyhow::Result<ChangeEnvelope> {
        Ok(input)
    }
}

pub struct ConsumerFactory;

impl sdk::Factory for ConsumerFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/input-binding-consumer", "1")
                .expect("fixture identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: sdk::ConfigSchema::from_fields([(
                "observations",
                sdk::ConfigType::String,
                true,
            )]),
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("fixture port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: sdk::Capabilities {
                input_binding_updates: true,
                ..Default::default()
            },
        }
    }

    fn consumer_mode(&self) -> Option<sdk::ConsumerMode> {
        Some(sdk::ConsumerMode::External)
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let observations = request
            .configuration
            .get("observations")
            .and_then(serde_json::Value::as_str)
            .context("missing observations file")?;
        Ok(sdk::Component::Consumer(Box::new(Consumer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            observations: PathBuf::from(observations),
            bound: false,
        }))
        .into())
    }
}

struct Consumer {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    observations: PathBuf,
    bound: bool,
}

#[async_trait]
impl sdk::NativeComponent for Consumer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }

    fn supports_input_binding_updates(&self) -> bool {
        true
    }

    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        std::fs::write(&self.observations, serde_json::to_vec(bindings)?)?;
        self.bound = true;
        Ok(())
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.bound, "inputs were not bound before start");
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl DeliveryHandler for Consumer {
    async fn handle(&mut self, _: DeliveryItem<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}
