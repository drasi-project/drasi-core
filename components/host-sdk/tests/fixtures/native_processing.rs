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
                erases_owned_state: true,
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        services: sdk::ServiceBindings,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let processing = services
            .processing
            .ok_or_else(|| anyhow::anyhow!("fixture requires its declared processing binding"))?;
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
            self.processing
                .run(|context| async move {
                    context
                        .delete_element(&ElementReference::new("source", "1"))
                        .await?;
                    Ok(())
                })
                .await
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
