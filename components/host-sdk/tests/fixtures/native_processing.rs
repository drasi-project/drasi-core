// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentRole, GraphChangeCodec, ImplementationIdentity, InputEnvelope,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, StreamId,
};
use drasi_computation_plugin_sdk as sdk;
use drasi_core::models::{ElementReference, SourceChange};

pub struct Factory;

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/indexed-processing", "1")
                .expect("fixture identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: sdk::ConfigSchema::from_fields([(
                "fail",
                sdk::ConfigType::Boolean,
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
                indexed_processing: true,
                deprovision: true,
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        _: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        anyhow::bail!("fixture requires host-owned indexed processing")
    }

    fn create_with_processing(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        processing: sdk::NativeProcessing,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        Ok(sdk::Component::Transformer(Box::new(Transformer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            fail: request.configuration["fail"]
                .as_bool()
                .context("missing fail configuration")?,
            processing,
        }))
        .into())
    }
}

struct Transformer {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    fail: bool,
    processing: sdk::NativeProcessing,
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
        self.processing
            .run(|context| async move {
                context
                    .delete_element(&ElementReference::new("source", "1"))
                    .await?;
                Ok(())
            })
            .await
    }
}

#[async_trait]
impl sdk::NativeTransformer for Transformer {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        let [SourceChange::Insert { element }] = changes.as_slice() else {
            anyhow::bail!("fixture expects exactly one insertion");
        };
        let element = element.clone();
        let fail = self.fail;
        self.processing
            .run(move |context| async move {
                context.set_element(&element, &[0]).await?;
                anyhow::ensure!(
                    context.get_element(element.get_reference()).await? == Some(element.clone()),
                    "read-your-writes was not preserved"
                );
                anyhow::ensure!(
                    context
                        .get_slot_element(0, element.get_reference())
                        .await?
                        .is_some(),
                    "slot membership was not preserved"
                );
                anyhow::ensure!(!fail, "intentional fixture rollback");
                Ok(())
            })
            .await?;
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: GraphChangeCodec::derive_changes(
                &input.envelope,
                &changes,
                StreamId::try_new(format!("{}/out", self.descriptor.id()))?,
                input.envelope.system().sequence(),
            )?,
        }])
    }
}
