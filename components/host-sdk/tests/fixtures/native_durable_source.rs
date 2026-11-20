// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A source that declares a restart-stable, replayable output and emits
//! nothing itself: hosts inject envelopes carrying a persistent producer
//! identity, so exactly-once consumers may be connected to it.

use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentRole, GraphChangeCodec, ImplementationIdentity, OutputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{self as sdk, lifecycle};

pub struct Factory;

impl sdk::Factory for Factory {
    fn configuration_details(&self) -> sdk::ConfigDetails {
        sdk::ConfigDetails {
            fields: [(
                "stream".to_string(),
                sdk::configuration::ConfigFieldDetails {
                    description: "Output stream.".into(),
                    ..Default::default()
                },
            )]
            .into(),
        }
    }
    fn describe(
        &self,
        request: &sdk::wire::DescribeRequest,
    ) -> anyhow::Result<sdk::ComponentDescription> {
        Ok(request.source_description(
            sdk::OutputDescription::DURABLE,
            sdk::ProgressPersistence::Durable,
        ))
    }
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/durable-source", "1")
                .expect("fixture identity"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: [(
                    "stream".into(),
                    sdk::ConfigField {
                        constraint: None,
                        value_type: sdk::ConfigType::String,
                        required: true,
                        secret: false,
                    },
                )]
                .into(),
                allow_additional: false,
            },
            ports: vec![PortDescriptor::new(
                PortId::try_new("out").expect("fixture port"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: None,
            capabilities: sdk::Capabilities::default(),
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        _: sdk::ServiceBindings,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        Ok(sdk::Component::Source(Box::new(Source {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
        }))
        .into())
    }
}

struct Source {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
}

#[async_trait]
impl sdk::NativeComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    async fn initialize(&mut self, _: &lifecycle::InitializeContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn start(&mut self, _: &mut lifecycle::StartContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn pause(&mut self, _: &lifecycle::PauseContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn resume(&mut self, _: &lifecycle::ResumeContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn stop(&mut self, _: &lifecycle::StopContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn dispose(&mut self, _: &lifecycle::DisposeContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}
