// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::lifecycle::{DisposeReason, StateIntent, StopMode};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity,
    InputBinding, InputEnvelope, OutputEndpoint, OutputEnvelope, PipeRequirements, PortDescriptor,
    PortId, StreamId,
};

struct TestFactory {
    advertised: bool,
    implemented: bool,
    seen: Arc<Mutex<Vec<Vec<InputBinding>>>>,
}

impl Factory for TestFactory {
    fn configuration_details(&self) -> crate::ConfigDetails {
        Default::default()
    }
    fn describe(
        &self,
        _request: &crate::wire::DescribeRequest,
    ) -> anyhow::Result<crate::ComponentDescription> {
        Ok(crate::ComponentDescription::undeclared())
    }
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/input-bindings", "1").unwrap(),
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
                input_binding_updates: self.advertised,
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        _services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Transformer(Box::new(TestTransformer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            implemented: self.implemented,
            seen: self.seen.clone(),
        }))
        .into())
    }
}

struct TestTransformer {
    descriptor: ComponentDescriptor,
    implemented: bool,
    seen: Arc<Mutex<Vec<Vec<InputBinding>>>>,
}

#[async_trait]
impl ComputationComponent for TestTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }

    fn input_bindings(&mut self) -> Option<&mut dyn crate::NativeInputBindings> {
        self.implemented
            .then_some(self as &mut dyn crate::NativeInputBindings)
    }

    crate::passive_lifecycle!();
}

#[async_trait]
impl crate::NativeInputBindings for TestTransformer {
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        anyhow::ensure!(
            !bindings
                .iter()
                .any(|binding| binding.stream.as_str() == "reject"),
            "injected input-binding failure"
        );
        self.seen.lock().unwrap().push(bindings.to_vec());
        Ok(())
    }
}

#[async_trait]
impl Transformer for TestTransformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}

const ID: &str = "5e5157b1-10c8-4ad3-908e-b30a502ef1b4";

fn context(code: u32) -> Vec<u8> {
    let context = wire::LifecycleRequest::new("lifecycle-operation", ID, 1);
    wire::encode(&match code {
        abi::operation::STOP => context.with_stop_mode(StopMode::Drain),
        abi::operation::DISPOSE => {
            context.with_dispose(DisposeReason::Shutdown, StateIntent::Preserve, 1)
        }
        _ => context,
    })
    .unwrap()
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

    async fn run(&self, code: u32, input: &[u8]) -> Result<(), Failure> {
        let output = self.begin(code, input)?.await?;
        assert!(output.is_empty());
        Ok(())
    }
}

impl Drop for RawComponent {
    fn drop(&mut self) {
        unsafe { (*self.0.vtable).release.expect("component release")(self.0.state) }
    }
}

fn raw_component(factory: TestFactory) -> anyhow::Result<RawComponent> {
    let exported = ExportedPlugin::new(PluginDefinition::new(
        "fixture/direct-input-bindings",
        "1",
        vec![Arc::new(factory)],
        vec![GraphChangeCodec::schema()],
    )?)?;
    let metadata = &exported.state.definition.metadata.factories[0];
    let request = wire::encode(&CreateRequest {
        id: ComponentId::try_new(ID)?,
        implementation: metadata.implementation.clone(),
        configuration_version: 1,
        configuration: serde_json::json!({}),
        scope: None,
        inputs: Vec::new(),
        activation_credentials_bound: false,
        update_directive: None,
    })?;
    let mut plugin = abi::PluginHandle::null();
    let mut factory = abi::FactoryHandle::null();
    let mut component = abi::ComponentHandle::null();
    unsafe {
        exported.entry(&mut plugin)?;
        transport::take_status((*plugin.vtable).factory.expect("factory")(
            plugin.state,
            0,
            &mut factory,
        ))?;
        let status = (*factory.vtable).create.expect("create")(
            factory.state,
            BorrowedBytes::new(&request),
            &abi::services::ServiceBindingsV1::none(),
            &mut component,
        );
        (*factory.vtable).release.expect("factory release")(factory.state);
        (*plugin.vtable).release.expect("plugin release")(plugin.state);
        transport::take_status(status)?;
    }
    Ok(RawComponent(component))
}

fn binding(input: &str, producer: &str, stream: &str) -> InputBinding {
    InputBinding {
        input_port: PortId::try_new(input).unwrap(),
        output: OutputEndpoint::new(
            ComponentId::try_new(producer).unwrap(),
            PortId::try_new("out").unwrap(),
        ),
        stream: StreamId::try_new(stream).unwrap(),
    }
}

const FIRST: &str = "c789ffdd-ea9c-429f-968a-eef534899eae";
const SECOND: &str = "8454c4a2-d342-4ab8-b2cc-ab381334ed22";

#[tokio::test]
async fn binding_updates_work_before_start_and_during_processing() -> anyhow::Result<()> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let component = raw_component(TestFactory {
        advertised: true,
        implemented: true,
        seen: seen.clone(),
    })?;
    let first = vec![binding("in", FIRST, "first/out")];
    let second = vec![binding("in", SECOND, "second/out")];
    component
        .run(abi::operation::BIND_INPUTS, &wire::encode(&first)?)
        .await?;
    component
        .run(
            abi::operation::INITIALIZE,
            &context(abi::operation::INITIALIZE),
        )
        .await?;
    component
        .run(abi::operation::START, &context(abi::operation::START))
        .await?;
    for _ in 0..2 {
        component
            .run(abi::operation::BIND_INPUTS, &wire::encode(&second)?)
            .await?;
    }
    let invalid = vec![binding("missing", FIRST, "first/out")];
    let error = component
        .run(abi::operation::BIND_INPUTS, &wire::encode(&invalid)?)
        .await
        .expect_err("undeclared input port must fail");
    assert!(error.message.contains("undeclared input port"));
    let rejected = vec![binding("in", FIRST, "reject")];
    let error = component
        .run(abi::operation::BIND_INPUTS, &wire::encode(&rejected)?)
        .await
        .expect_err("component failure must propagate");
    assert!(error.message.contains("injected input-binding failure"));
    assert!(component
        .run(abi::operation::BIND_INPUTS, b"not messagepack")
        .await
        .is_err());
    component
        .run(abi::operation::STOP, &context(abi::operation::STOP))
        .await?;
    component
        .run(abi::operation::BIND_INPUTS, &wire::encode(&first)?)
        .await?;
    assert_eq!(
        *seen.lock().unwrap(),
        vec![first.clone(), second.clone(), second, first]
    );
    component
        .run(abi::operation::DISPOSE, &context(abi::operation::DISPOSE))
        .await?;
    Ok(())
}

#[test]
fn input_binding_updates_require_matching_capability_and_supported_role() -> anyhow::Result<()> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let component = raw_component(TestFactory {
        advertised: false,
        implemented: false,
        seen: seen.clone(),
    })?;
    assert_eq!(
        component
            .begin(
                abi::operation::BIND_INPUTS,
                &wire::encode(&Vec::<InputBinding>::new())?
            )
            .err()
            .expect("undeclared operation must fail")
            .code,
        abi::status::UNSUPPORTED
    );
    for (advertised, implemented) in [(true, false), (false, true)] {
        let error = raw_component(TestFactory {
            advertised,
            implemented,
            seen: seen.clone(),
        })
        .err()
        .expect("mismatched capability must fail construction");
        assert!(error
            .to_string()
            .contains("input-binding handler does not match its declared capability"));
    }
    let mut metadata = TestFactory {
        advertised: true,
        implemented: true,
        seen,
    }
    .metadata();
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
        .expect_err("source input updates are invalid");
    assert!(error.to_string().contains("input-binding updates require"));
    Ok(())
}
