// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentRole, GraphChangeCodec, ImplementationIdentity, InputEnvelope,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk as sdk;
use std::path::PathBuf;

pub struct Factory;

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/stateful-transformer", "1")
                .expect("fixture identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: [(
                    "state_file".into(),
                    sdk::ConfigField {
                        value_type: sdk::ConfigType::String,
                        required: true,
                        secret: false,
                    },
                )]
                .into_iter()
                .collect(),
                allow_additional: false,
            },
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
                deprovision: true,
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let state_file = request
            .configuration
            .get("state_file")
            .and_then(serde_json::Value::as_str)
            .context("missing state file")?;
        Ok(sdk::Component::Transformer(Box::new(Transformer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            state_file: PathBuf::from(state_file),
            configuration: request.configuration.clone(),
        }))
        .into())
    }
}

struct Transformer {
    descriptor: ComponentDescriptor,
    state_file: PathBuf,
    configuration: serde_json::Value,
}

#[async_trait]
impl sdk::NativeComponent for Transformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }

    fn supports_deprovision(&self) -> bool {
        true
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn deprovision(&mut self) -> anyhow::Result<()> {
        match std::fs::remove_file(&self.state_file) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove fixture-owned state"),
        }
    }
}

#[async_trait]
impl sdk::NativeTransformer for Transformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}
