// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! One declaration per capability, validated when the plugin is defined; all
//! bound Host services delivered together through the single create entry;
//! capability hooks required by their typed handlers.

use super::*;
use crate::lifecycle::{DisposeReason, StateIntent, StopMode};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity,
    InputEnvelope, OutputEnvelope, PipeRequirements, PortDescriptor, PortId,
};
use std::sync::atomic::AtomicUsize;

const ID: &str = "3a1c51a4-5d9d-4f1b-93a0-7a1e22f5d4c1";

fn port(name: &str, direction: PortDirection) -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new(name).unwrap(),
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}

/// What a test factory declares and implements, independently.
#[derive(Clone, Default)]
struct Shape {
    role: Option<ComponentRole>,
    services: crate::HostServices,
    capabilities: crate::Capabilities,
    planner: bool,
    credentials: bool,
    coordination: bool,
    described: bool,
    durable_handler: bool,
}

#[derive(Default)]
struct Seen {
    processing: AtomicUsize,
    snapshots: AtomicUsize,
    durable_deliveries: AtomicUsize,
    base_deliveries: AtomicUsize,
}

struct ShapedFactory {
    shape: Shape,
    seen: Arc<Seen>,
}

impl crate::UpdatePlanner for ShapedFactory {
    fn assess(&self, _: &crate::UpdateRequest) -> anyhow::Result<crate::lifecycle::Assessment> {
        anyhow::bail!("not assessed in these tests")
    }
}

impl crate::ActivationCredentialEndpoint for ShapedFactory {
    fn activation_credential_context(
        &self,
        _: &CreateRequest,
    ) -> anyhow::Result<crate::ActivationCredentialContext> {
        anyhow::bail!("not inspected in these tests")
    }
}

impl crate::SourceBootstrapCoordination for ShapedFactory {
    fn create_bootstrap_coordinator(
        &self,
        _: &CreateRequest,
        _: crate::SourceBootstrapProgress,
    ) -> anyhow::Result<Arc<dyn crate::NativeSourceBootstrapCoordinator>> {
        anyhow::bail!("not coordinated in these tests")
    }
}

impl Factory for ShapedFactory {
    fn metadata(&self) -> FactoryMetadata {
        let role = self.shape.role.unwrap_or(ComponentRole::Transformer);
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/services", "1").unwrap(),
            role,
            configuration_version: 1,
            configuration: crate::ConfigSchema::from_fields([(
                "value",
                crate::ConfigType::Integer,
                false,
            )]),
            ports: match role {
                ComponentRole::Source => vec![port("out", PortDirection::Output)],
                ComponentRole::Sink => vec![port("in", PortDirection::Input)],
                _ => vec![
                    port("in", PortDirection::Input),
                    port("out", PortDirection::Output),
                ],
            },
            completion: (role == ComponentRole::Sink)
                .then_some(drasi_computation_contracts::SinkCompletion::Handled),
            capabilities: crate::Capabilities {
                services: self.shape.services.clone(),
                ..self.shape.capabilities.clone()
            },
        }
    }
    fn configuration_details(&self) -> crate::ConfigDetails {
        let mut details = crate::ConfigDetails::default();
        if self.shape.described {
            details.fields.insert(
                "value".into(),
                crate::configuration::ConfigFieldDetails {
                    description: "A test value.".into(),
                    ..Default::default()
                },
            );
        }
        details
    }
    fn describe(
        &self,
        _: &crate::wire::DescribeRequest,
    ) -> anyhow::Result<crate::ComponentDescription> {
        Ok(crate::ComponentDescription::undeclared())
    }
    fn update_planner(&self) -> Option<&dyn crate::UpdatePlanner> {
        self.shape
            .planner
            .then_some(self as &dyn crate::UpdatePlanner)
    }
    fn activation_credentials(&self) -> Option<&dyn crate::ActivationCredentialEndpoint> {
        self.shape
            .credentials
            .then_some(self as &dyn crate::ActivationCredentialEndpoint)
    }
    fn bootstrap_coordination(&self) -> Option<&dyn crate::SourceBootstrapCoordination> {
        self.shape
            .coordination
            .then_some(self as &dyn crate::SourceBootstrapCoordination)
    }
    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        if services.processing.is_some() {
            self.seen.processing.fetch_add(1, Ordering::SeqCst);
        }
        if services.output_snapshots.is_some() {
            self.seen.snapshots.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Component::Transformer(Box::new(ShapedTransformer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            durable_handler: self.shape.durable_handler,
            seen: self.seen.clone(),
        }))
        .into())
    }
}

struct ShapedTransformer {
    descriptor: ComponentDescriptor,
    durable_handler: bool,
    seen: Arc<Seen>,
}

#[async_trait]
impl ComputationComponent for ShapedTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    crate::passive_lifecycle!();
}

#[async_trait]
impl crate::NativeDurableOutput for ShapedTransformer {
    async fn delivery_completed(&mut self, _: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.seen.durable_deliveries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn resume_delivery(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for ShapedTransformer {
    fn durable_output(&mut self) -> Option<&mut dyn crate::NativeDurableOutput> {
        self.durable_handler
            .then_some(self as &mut dyn crate::NativeDurableOutput)
    }
    fn has_pending_emissions(&self) -> bool {
        false
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
    async fn delivery_completed(&mut self, _: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.seen.base_deliveries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}

fn define(shape: Shape) -> anyhow::Result<PluginDefinition> {
    define_with(shape, Arc::default())
}

fn define_with(shape: Shape, seen: Arc<Seen>) -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        "fixture/services",
        "1",
        vec![Arc::new(ShapedFactory { shape, seen })],
        vec![GraphChangeCodec::schema()],
    )
}

fn described() -> Shape {
    Shape {
        described: true,
        ..Shape::default()
    }
}

fn refusal(shape: Shape) -> String {
    format!(
        "{:#}",
        define(shape).err().expect("the definition must be refused")
    )
}

#[test]
fn a_claimed_factory_implementation_that_is_missing_is_refused_when_defined() {
    define(described()).expect("a consistent factory is accepted");
    let source = || Shape {
        role: Some(ComponentRole::Source),
        services: crate::HostServices {
            source_progress: true,
            bootstrap_coordination: true,
            ..Default::default()
        },
        coordination: true,
        ..described()
    };
    define(source()).expect("declared coordination with its implementation");
    let error = refusal(Shape {
        coordination: false,
        ..source()
    });
    assert!(
        error.contains("bootstrap coordination is declared"),
        "{error}"
    );
    let error = refusal(Shape {
        credentials: true,
        ..described()
    });
    assert!(
        error.contains("activation credentials is not declared"),
        "{error}"
    );
    let error = refusal(Shape {
        services: crate::HostServices {
            activation_credentials: true,
            ..Default::default()
        },
        ..described()
    });
    assert!(
        error.contains("activation credentials is declared"),
        "{error}"
    );
    let error = refusal(Shape {
        capabilities: crate::Capabilities {
            update_modes: vec![crate::ComponentUpdateMode::Recreate],
            ..Default::default()
        },
        ..described()
    });
    assert!(error.contains("update planning"), "{error}");
    let error = refusal(Shape {
        planner: true,
        ..described()
    });
    assert!(error.contains("update planning"), "{error}");
}

#[test]
fn configuration_details_must_describe_every_field() {
    let error = refusal(Shape::default());
    assert!(
        error.contains("does not describe configuration field value"),
        "{error}"
    );
}

#[test]
fn only_contradictory_service_declarations_are_refused_when_defined() {
    let source = |services| Shape {
        role: Some(ComponentRole::Source),
        services,
        ..described()
    };
    for (services, reason) in [
        (
            crate::HostServices {
                source_progress: true,
                consumer_progress: true,
                ..Default::default()
            },
            "both single-owner and consumer-group progress",
        ),
        (
            crate::HostServices {
                source_admission: true,
                source_progress: true,
                ..Default::default()
            },
            "source admission owns the source's replay",
        ),
        (
            crate::HostServices {
                source_admission: true,
                consumer_progress: true,
                ..Default::default()
            },
            "source admission owns the source's replay",
        ),
        (
            crate::HostServices {
                bootstrap_coordination: true,
                ..Default::default()
            },
            "bootstrap coordination requires a source declaring one progress mode",
        ),
    ] {
        let error = refusal(source(services));
        assert!(error.contains(reason), "{error}");
    }
    let sink = |services| Shape {
        role: Some(ComponentRole::Sink),
        services,
        ..described()
    };
    let error = refusal(sink(crate::HostServices {
        consumer: Some(crate::ConsumerMode::External),
        sink_batching: true,
        ..Default::default()
    }));
    assert!(
        error.contains("cannot also declare Host-owned batching"),
        "{error}"
    );
    // Services that do not contradict each other are declared together.
    define(sink(crate::HostServices {
        consumer: Some(crate::ConsumerMode::External),
        output_snapshot_reads: true,
        ..Default::default()
    }))
    .expect("a consumer may also read output snapshots");
    define(sink(crate::HostServices {
        sink_batching: true,
        output_snapshot_reads: true,
        ..Default::default()
    }))
    .expect("a batching sink may also read output snapshots");
    define(Shape {
        services: crate::HostServices {
            consumer_progress: true,
            output_snapshot_reads: true,
            ..Default::default()
        },
        ..described()
    })
    .expect("a transformer may read consumer progress and output snapshots");
    // Role rules are declared facts too.
    let error = refusal(Shape {
        services: crate::HostServices {
            source_progress: true,
            ..Default::default()
        },
        ..described()
    });
    assert!(
        error.contains("source progress requires a native source"),
        "{error}"
    );
    let error = refusal(durable(crate::HostServices {
        consumer_progress: true,
        ..Default::default()
    }));
    assert!(
        error.contains("durable output owns retained output"),
        "{error}"
    );
}

/// An indexed transformer with durable output, plus `services`.
fn durable(services: crate::HostServices) -> Shape {
    Shape {
        services,
        capabilities: crate::Capabilities {
            indexed_processing: true,
            committed_input_progress: Some(PortId::try_new("in").unwrap()),
            durable_output: true,
            continuations: true,
            startup_continuation: true,
            ..Default::default()
        },
        durable_handler: true,
        ..described()
    }
}

// Inert Host service tables: construction retains and releases them only.
unsafe extern "C" fn retain(_: *mut c_void) {}
unsafe extern "C" fn release(_: *mut c_void) {}
unsafe extern "C" fn run(
    _: *mut c_void,
    _: *const abi::processing::ProcessingWorkV1,
    _: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_result(Err(Failure::protocol("inert processing")))
}
unsafe extern "C" fn open(
    _: *mut c_void,
    _: BorrowedBytes,
    _: *mut abi::output_snapshots::OutputSnapshotHandle,
) -> abi::Status {
    transport::status_result(Err(Failure::protocol("inert snapshots")))
}

static CONTEXT: u8 = 0;

fn processing_table() -> abi::processing::ProcessingV1 {
    abi::processing::ProcessingV1 {
        header: abi::Header::new::<abi::processing::ProcessingV1>(),
        context: (&CONTEXT as *const u8).cast_mut().cast(),
        retain: Some(retain),
        release: Some(release),
        run: Some(run),
    }
}

fn snapshots_table() -> abi::output_snapshots::OutputSnapshotsV1 {
    abi::output_snapshots::OutputSnapshotsV1 {
        header: abi::Header::new::<abi::output_snapshots::OutputSnapshotsV1>(),
        context: (&CONTEXT as *const u8).cast_mut().cast(),
        retain: Some(retain),
        release: Some(release),
        open: Some(open),
    }
}

struct RawComponent(abi::ComponentHandle);

impl RawComponent {
    async fn run(&self, code: u32, input: &[u8]) -> Result<Vec<u8>, Failure> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            transport::take_status((*self.0.vtable).begin.expect("component begin")(
                self.0.state,
                code,
                BorrowedBytes::new(input),
                std::ptr::null(),
                &mut operation,
            ))?;
            Ok(transport::OperationFuture::new(operation)?.await?.to_vec())
        }
    }
}

impl Drop for RawComponent {
    fn drop(&mut self) {
        unsafe { (*self.0.vtable).release.expect("component release")(self.0.state) }
    }
}

/// Construct through the single ABI create entry with `bindings`.
fn create(
    definition: PluginDefinition,
    bindings: abi::services::ServiceBindingsV1,
) -> anyhow::Result<RawComponent> {
    let exported = ExportedPlugin::new(definition)?;
    let metadata = exported.state.definition.metadata.factories[0].clone();
    let request = wire::encode(&CreateRequest {
        id: ComponentId::try_new(ID)?,
        implementation: metadata.implementation,
        configuration_version: 1,
        configuration: serde_json::json!({}),
        scope: Some(crate::Scope {
            instance_id: "instance".into(),
            graph_id: "graph".into(),
            generation: 1,
        }),
        inputs: Vec::new(),
        activation_credentials_bound: false,
        update_directive: None,
    })?;
    let mut plugin = abi::PluginHandle::null();
    let mut factory = abi::FactoryHandle::null();
    let mut component = abi::ComponentHandle::null();
    let result = unsafe {
        exported.entry(&mut plugin)?;
        transport::take_status((*plugin.vtable).factory.expect("factory")(
            plugin.state,
            0,
            &mut factory,
        ))?;
        let result = transport::take_status((*factory.vtable).create.expect("create")(
            factory.state,
            BorrowedBytes::new(&request),
            &bindings,
            &mut component,
        ));
        (*factory.vtable).release.expect("factory release")(factory.state);
        (*plugin.vtable).release.expect("plugin release")(plugin.state);
        result
    };
    result?;
    Ok(RawComponent(component))
}

fn lifecycle(code: u32) -> Vec<u8> {
    let context = wire::LifecycleRequest::new("service-operation", ID, 1);
    wire::encode(&match code {
        abi::operation::STOP => context.with_stop_mode(StopMode::Drain),
        abi::operation::DISPOSE => {
            context.with_dispose(DisposeReason::Shutdown, StateIntent::Preserve, 1)
        }
        _ => context,
    })
    .unwrap()
}

#[test]
fn every_bound_service_reaches_create_together() -> anyhow::Result<()> {
    let seen = Arc::new(Seen::default());
    let processing = processing_table();
    let snapshots = snapshots_table();
    let _component = create(
        define_with(
            durable(crate::HostServices {
                output_snapshot_reads: true,
                ..Default::default()
            }),
            seen.clone(),
        )?,
        abi::services::ServiceBindingsV1 {
            processing: &processing,
            output_snapshots: &snapshots,
            ..abi::services::ServiceBindingsV1::none()
        },
    )?;
    assert_eq!(seen.processing.load(Ordering::SeqCst), 1);
    assert_eq!(seen.snapshots.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn undeclared_and_missing_required_services_are_refused() -> anyhow::Result<()> {
    let processing = processing_table();
    let snapshots = snapshots_table();
    let error = create(
        define(durable(crate::HostServices::default()))?,
        abi::services::ServiceBindingsV1 {
            processing: &processing,
            output_snapshots: &snapshots,
            ..abi::services::ServiceBindingsV1::none()
        },
    )
    .err()
    .expect("snapshots are not declared");
    assert!(
        format!("{error:#}").contains("does not declare output snapshot reads"),
        "{error:#}"
    );
    let error = create(
        define(durable(crate::HostServices::default()))?,
        abi::services::ServiceBindingsV1::none(),
    )
    .err()
    .expect("indexed processing is required");
    assert!(
        format!("{error:#}").contains("indexed processing binding"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn a_durable_output_transformer_must_provide_its_delivery_handler() -> anyhow::Result<()> {
    let processing = processing_table();
    let error = create(
        define(Shape {
            durable_handler: false,
            ..durable(crate::HostServices::default())
        })?,
        abi::services::ServiceBindingsV1 {
            processing: &processing,
            ..abi::services::ServiceBindingsV1::none()
        },
    )
    .err()
    .expect("a durable transformer without its handler is refused");
    assert!(
        format!("{error:#}").contains("durable output handler"),
        "{error:#}"
    );
    let error = create(
        define(Shape {
            durable_handler: true,
            ..described()
        })?,
        abi::services::ServiceBindingsV1::none(),
    )
    .err()
    .expect("an undeclared durable handler is refused");
    assert!(
        format!("{error:#}").contains("durable output handler"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn delivery_completion_reaches_the_durable_handler_not_the_silent_default(
) -> anyhow::Result<()> {
    use abi::operation::{DELIVERY_COMPLETED, DISPOSE, INITIALIZE, START, STOP};
    let processing = processing_table();
    for durable_output in [true, false] {
        let seen = Arc::new(Seen::default());
        let shape = if durable_output {
            durable(crate::HostServices::default())
        } else {
            described()
        };
        let processing_binding = if durable_output {
            &processing as *const _
        } else {
            std::ptr::null()
        };
        let component = create(
            define_with(shape, seen.clone())?,
            abi::services::ServiceBindingsV1 {
                processing: processing_binding,
                ..abi::services::ServiceBindingsV1::none()
            },
        )?;
        component.run(INITIALIZE, &lifecycle(INITIALIZE)).await?;
        component.run(START, &lifecycle(START)).await?;
        let none: Vec<wire::Envelope> = Vec::new();
        component
            .run(DELIVERY_COMPLETED, &wire::encode(&none)?)
            .await?;
        assert_eq!(
            seen.durable_deliveries.load(Ordering::SeqCst),
            usize::from(durable_output)
        );
        assert_eq!(
            seen.base_deliveries.load(Ordering::SeqCst),
            usize::from(!durable_output)
        );
        component.run(STOP, &lifecycle(STOP)).await?;
        component.run(DISPOSE, &lifecycle(DISPOSE)).await?;
    }
    Ok(())
}
