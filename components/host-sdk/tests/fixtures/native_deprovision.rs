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
    /// Every field described (fixture).
    fn configuration_details(&self) -> sdk::ConfigDetails {
        sdk::ConfigDetails {
            fields: sdk::Factory::metadata(self)
                .configuration
                .fields
                .into_keys()
                .map(|name| {
                    let details = sdk::configuration::ConfigFieldDetails {
                        description: format!("Fixture field {name}."),
                        ..Default::default()
                    };
                    (name, details)
                })
                .collect(),
        }
    }
    fn describe(
        &self,
        _request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(drasi_computation_plugin_sdk::ComponentDescription::undeclared())
    }
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
                        constraint: None,
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
                erases_owned_state: true,
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        _services: sdk::ServiceBindings,
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

    async fn start(
        &mut self,
        _: &mut drasi_computation_plugin_sdk::lifecycle::StartContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }

    async fn stop(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::StopContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }

    async fn dispose(
        &mut self,
        ctx: &drasi_computation_plugin_sdk::lifecycle::DisposeContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        if !ctx.erases_owned_state() {
            return Ok(());
        }
        let result: anyhow::Result<()> = async {
            match std::fs::remove_file(&self.state_file) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("remove fixture-owned state"),
            }
        }
        .await;
        Ok(result?)
    }
    async fn initialize(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::InitializeContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }
    async fn pause(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::PauseContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }
    async fn resume(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::ResumeContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeTransformer for Transformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}
