// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentRole, ImplementationIdentity, InputEnvelope, OutputEnvelope,
    PortDescriptor, PortDirection, PortId, QueryChangeCodec, StreamId, SystemMetadata,
};
use drasi_computation_plugin_sdk::{self as sdk, NativeComponent, NativeTransformer};
use std::sync::Arc;

struct Factory;
impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/handled-branch", "1")
                .expect("literal"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: Default::default(),
                allow_additional: false,
            },
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(id, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(id).expect("literal"),
                        direction,
                        QueryChangeCodec::schema().descriptor().clone(),
                        Default::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: sdk::metadata::Capabilities {
                continuations: true,
                ..Default::default()
            },
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Transformer(Box::new(Transformer {
                descriptor: self.metadata().descriptor(request.id.clone())?,
                input: None,
                sequence: 0,
            })),
            control_handler: None,
        })
    }
}

struct Transformer {
    descriptor: ComponentDescriptor,
    input: Option<InputEnvelope>,
    sequence: u64,
}
#[async_trait]
impl NativeComponent for Transformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.input = None;
        Ok(())
    }
}
#[async_trait]
impl NativeTransformer for Transformer {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(self.input.is_none(), "fixture input already retained");
        self.input = Some(input);
        Ok(Vec::new())
    }
    fn has_pending_emissions(&self) -> bool {
        self.input.is_some()
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| anyhow::anyhow!("fixture input missing"))?;
        self.sequence += 1;
        let stream = StreamId::try_new("native/out")?;
        let envelope = input.envelope.derive(
            drasi_computation_contracts::emission_id(&stream, self.sequence)?,
            input.envelope.changes().clone(),
            SystemMetadata::new(stream, self.sequence),
        );
        Ok(vec![OutputEnvelope::new(PortId::try_new("out")?, envelope)
            .with_execution_context(input.execution_context)])
    }
}

pub(crate) fn definition() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "test/handled-branch-fixture",
        "1",
        vec![Arc::new(Factory)],
        vec![QueryChangeCodec::schema()],
    )
}
