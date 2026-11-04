// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity,
    InputEnvelope, OutputEnvelope, PipeRequirements, PortDescriptor, PortId,
};
use std::sync::atomic::AtomicUsize;

struct TestFactory(Arc<AtomicUsize>);

impl Factory for TestFactory {
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/deprovision", "1").unwrap(),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: Default::default(),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(name, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(name).unwrap(),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: crate::Capabilities {
                deprovision: true,
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Transformer(Box::new(TestTransformer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            deprovisions: self.0.clone(),
        }))
        .into())
    }
}

struct TestTransformer {
    descriptor: ComponentDescriptor,
    deprovisions: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for TestTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
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
        self.deprovisions.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Transformer for TestTransformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}

struct RawComponent(abi::ComponentHandle);

impl RawComponent {
    fn begin(&self, code: u32, input: &[u8]) -> Result<transport::OperationFuture, Failure> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            transport::take_status((*self.0.vtable).begin.expect("component begin")(
                self.0.state,
                code,
                BorrowedBytes::new(input),
                std::ptr::null(),
                &mut operation,
            ))?;
            transport::OperationFuture::new(operation)
        }
    }

    async fn run(&self, code: u32) -> Result<(), Failure> {
        let output = self.begin(code, &[])?.await?;
        assert!(output.is_empty());
        Ok(())
    }
}

impl Drop for RawComponent {
    fn drop(&mut self) {
        unsafe { (*self.0.vtable).release.expect("component release")(self.0.state) }
    }
}

fn raw_component(deprovisions: Arc<AtomicUsize>) -> anyhow::Result<RawComponent> {
    let factory = Arc::new(TestFactory(deprovisions));
    let exported = ExportedPlugin::new(PluginDefinition::new(
        "fixture/direct-deprovision",
        "1",
        vec![factory],
        vec![GraphChangeCodec::schema()],
    )?)?;
    let metadata = &exported.state.definition.metadata.factories[0];
    let mut plugin = abi::PluginHandle::null();
    let mut factory = abi::FactoryHandle::null();
    let mut component = abi::ComponentHandle::null();
    let request = wire::encode(&CreateRequest {
        id: ComponentId::try_new("direct")?,
        implementation: metadata.implementation.clone(),
        configuration_version: 1,
        configuration: serde_json::json!({}),
        scope: None,
        inputs: Vec::new(),
        activation_credentials_bound: false,
    })?;
    unsafe {
        exported.entry(&mut plugin)?;
        transport::take_status((*plugin.vtable).factory.expect("factory")(
            plugin.state,
            0,
            &mut factory,
        ))?;
        transport::take_status((*factory.vtable).create.expect("create")(
            factory.state,
            BorrowedBytes::new(&request),
            &mut component,
        ))?;
        (*factory.vtable).release.expect("factory release")(factory.state);
        (*plugin.vtable).release.expect("plugin release")(plugin.state);
    }
    Ok(RawComponent(component))
}

#[tokio::test(flavor = "current_thread")]
async fn direct_abi_fences_retirement_and_does_not_repeat_completed_deletion() -> anyhow::Result<()>
{
    let deprovisions = Arc::new(AtomicUsize::new(0));
    let component = raw_component(deprovisions.clone())?;
    component.run(abi::operation::START).await?;
    assert_eq!(
        component
            .begin(abi::operation::DEPROVISION, &[])
            .err()
            .expect("running deletion is rejected")
            .code,
        abi::status::FAILED
    );
    component.run(abi::operation::STOP).await?;
    assert_eq!(
        component
            .begin(abi::operation::DEPROVISION, b"unexpected")
            .err()
            .expect("deletion input is rejected")
            .code,
        abi::status::PROTOCOL
    );
    component.run(abi::operation::START).await?;
    component.run(abi::operation::STOP).await?;

    let cancelled = component.begin(abi::operation::DEPROVISION, &[])?;
    drop(cancelled);
    assert_eq!(
        component
            .begin(abi::operation::START, &[])
            .err()
            .expect("retirement fences restart")
            .code,
        abi::status::CLOSED
    );
    component.run(abi::operation::DEPROVISION).await?;
    component.run(abi::operation::DEPROVISION).await?;
    assert_eq!(deprovisions.load(Ordering::SeqCst), 1);
    assert_eq!(
        component
            .begin(abi::operation::STOP, &[])
            .err()
            .expect("retirement fences stop")
            .code,
        abi::status::CLOSED
    );
    Ok(())
}

#[test]
fn non_transformers_cannot_advertise_deprovision() {
    let mut metadata = TestFactory(Arc::new(AtomicUsize::new(0))).metadata();
    let plugin = PluginIdentity {
        id: "fixture".into(),
        version: "1".into(),
    };
    metadata.implementation.plugin = Some(plugin.clone());
    metadata.role = ComponentRole::Source;
    metadata
        .ports
        .retain(|port| port.direction() == PortDirection::Output);
    let error = metadata
        .validate(&plugin, &[GraphChangeCodec::schema().descriptor().clone()])
        .expect_err("source deprovision is not supported");
    assert!(error.to_string().contains("requires a transformer"));
}
