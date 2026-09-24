// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! These tests call the exported C entry/vtables, not a Rust factory shortcut.
//! Separately built cdylib conformance is exercised by the native plugin fixture.

use super::*;
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk as sdk;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::computation::v1::*;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn port(direction: PortDirection) -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new(if direction == PortDirection::Input {
            "in"
        } else {
            "out"
        })
        .unwrap(),
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}
fn envelope(component: &ComponentId, sequence: u64) -> ChangeEnvelope {
    let mut envelope = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &sequence.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::from(json!({"value": sequence})),
            },
        },
        StreamId::try_new(format!("{component}/out")).unwrap(),
        sequence,
        None,
    )
    .unwrap();
    envelope
        .append_annotation(
            ContextEntry::try_new(
                component.clone(),
                "test.context",
                ContextValue::Bytes(Arc::from([0u8, 255u8])),
            )
            .unwrap(),
        )
        .unwrap();
    envelope
}

struct TestFactory {
    role: ComponentRole,
    name: &'static str,
    controlled: bool,
    transactional: bool,
}
impl sdk::Factory for TestFactory {
    fn metadata(&self) -> FactoryMetadata {
        let ports = match self.role {
            ComponentRole::Source => vec![port(PortDirection::Output)],
            ComponentRole::Transformer => {
                vec![port(PortDirection::Input), port(PortDirection::Output)]
            }
            ComponentRole::Sink => vec![port(PortDirection::Input)],
            ComponentRole::Service => vec![],
            ComponentRole::Query => unreachable!(),
        };
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(self.name, "1").unwrap(),
            role: self.role,
            configuration_version: 1,
            configuration: ConfigSchema {
                fields: BTreeMap::from([(
                    "count".into(),
                    ConfigField {
                        value_type: ConfigType::Integer,
                        required: false,
                        secret: false,
                    },
                )]),
                allow_additional: true,
            },
            ports,
            completion: (self.role == ComponentRole::Sink).then_some(SinkCompletion::Handled),
            capabilities: Capabilities {
                control: self.controlled,
                transactional: self.transactional,
                ..Capabilities::default()
            },
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        anyhow::ensure!(
            request.configuration["fail_create"] != true,
            "requested constructor failure"
        );
        let notification = Arc::new(tokio::sync::Notify::new());
        let mut ports = sdk::Factory::metadata(self).ports;
        if request.configuration["wrong_descriptor"] == true {
            ports.clear();
        }
        let component = Box::new(TestComponent {
            descriptor: ComponentDescriptor::try_new(request.id.clone(), ports)?,
            configuration: request.configuration.clone(),
            seen: 0,
            notification: notification.clone(),
            controlled: self.controlled,
        });
        Ok(sdk::CreatedComponent {
            component: match self.role {
                ComponentRole::Source => sdk::Component::Source(component),
                ComponentRole::Transformer if self.transactional => {
                    sdk::Component::Transactional(component)
                }
                ComponentRole::Transformer => sdk::Component::Transformer(component),
                ComponentRole::Sink => sdk::Component::Sink(component),
                ComponentRole::Service => sdk::Component::Service(component),
                _ => unreachable!(),
            },
            control_handler: self.controlled.then(|| {
                Arc::new(ReleaseHandler(notification)) as Arc<dyn sdk::NativeControlHandler>
            }),
        })
    }
}
struct TestComponent {
    descriptor: ComponentDescriptor,
    configuration: Value,
    seen: u64,
    notification: Arc<tokio::sync::Notify>,
    controlled: bool,
}
#[async_trait]
impl ComputationComponent for TestComponent {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<Value> {
        let mut configuration = self.configuration.clone();
        configuration["seen"] = json!(self.seen);
        Ok(configuration)
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.configuration["fail_start"] != true,
            "requested start failure"
        );
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for TestComponent {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if self.configuration["blocked"] == true {
            std::future::pending::<()>().await;
        }
        if self.seen >= self.configuration["count"].as_u64().unwrap_or(2) {
            return Ok(None);
        }
        if self.controlled && self.seen > 0 {
            self.notification.notified().await;
        }
        // Timer creation occurs while the native poll enters the plugin runtime.
        tokio::time::sleep(Duration::from_millis(2)).await;
        self.seen += 1;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: envelope(self.descriptor.id(), self.seen),
        }))
    }
}
#[async_trait]
impl Transformer for TestComponent {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.configuration["fail_transform"] != true,
            "requested transform failure"
        );
        self.seen += 1;
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        let output = GraphChangeCodec::derive_changes(
            &input.envelope,
            &changes,
            StreamId::try_new(format!("{}/out", self.descriptor.id()))?,
            self.seen,
        )?;
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: output,
        }])
    }
}
#[async_trait]
impl EnvelopeSink for TestComponent {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.seen += GraphChangeCodec::decode_changes(&input.envelope)?.len() as u64;
        Ok(())
    }
}
#[async_trait]
impl ComputationService for TestComponent {
    async fn run(&mut self) -> anyhow::Result<()> {
        tokio::time::sleep(Duration::from_millis(2)).await;
        self.seen += 1;
        Ok(())
    }
}
#[async_trait]
impl sdk::TransactionalComponent for TestComponent {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }
    fn transaction_output_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }
    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &sdk::NativeTransactionContext<'_>,
    ) -> anyhow::Result<ChangeEnvelope> {
        use drasi_core::models::ElementValue;
        context.put("seen", ElementValue::Integer(1)).await?;
        anyhow::ensure!(
            context.get("seen").await? == Some(ElementValue::Integer(1)),
            "missing staged state"
        );
        context.derive(&input, input.changes().clone()).await
    }
}
struct ReleaseHandler(Arc<tokio::sync::Notify>);
#[async_trait]
impl sdk::NativeControlHandler for ReleaseHandler {
    async fn on_message(
        &self,
        _: sdk::ControlMessage,
        _: sdk::ControlSender,
    ) -> anyhow::Result<()> {
        self.0.notify_one();
        Ok(())
    }
}
fn definition() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "native-boundary-test",
        "1",
        vec![
            Arc::new(TestFactory {
                role: ComponentRole::Source,
                name: "test/source",
                controlled: false,
                transactional: false,
            }),
            Arc::new(TestFactory {
                role: ComponentRole::Transformer,
                name: "test/transformer",
                controlled: false,
                transactional: false,
            }),
            Arc::new(TestFactory {
                role: ComponentRole::Sink,
                name: "test/sink",
                controlled: false,
                transactional: false,
            }),
            Arc::new(TestFactory {
                role: ComponentRole::Service,
                name: "test/service",
                controlled: false,
                transactional: false,
            }),
            Arc::new(TestFactory {
                role: ComponentRole::Source,
                name: "test/controlled",
                controlled: true,
                transactional: false,
            }),
            Arc::new(TestFactory {
                role: ComponentRole::Transformer,
                name: "test/transaction",
                controlled: false,
                transactional: true,
            }),
        ],
        vec![GraphChangeCodec::schema()],
    )
}
sdk::export_computation_plugin!(definition());
fn plugin() -> Arc<NativePlugin> {
    unsafe {
        NativePlugin::from_entry_points(
            drasi_computation_plugin_metadata,
            drasi_computation_plugin_entry,
        )
    }
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn real_c_interfaces_preserve_envelopes_configuration_and_service_lifecycle() {
    let plugin = plugin();
    assert_eq!(plugin.metadata().abi_version, abi::ABI_VERSION);
    assert_eq!(plugin.transactional_factories().len(), 1);
    let mut source = plugin.factories()[0]
        .create_component(ComponentId::try_new("s").unwrap(), json!({"count":2}))
        .unwrap();
    let mut transformer = plugin.factories()[1]
        .create_component(ComponentId::try_new("t").unwrap(), json!({}))
        .unwrap();
    let mut sink = plugin.factories()[2]
        .create_component(ComponentId::try_new("k").unwrap(), json!({}))
        .unwrap();
    assert_eq!(source.configuration().unwrap()["seen"], 0);
    assert!(
        source.next().await.is_err(),
        "data before successful start is not accepted"
    );
    source.start().await.unwrap();
    transformer.start().await.unwrap();
    sink.start().await.unwrap();
    for sequence in 1..=2 {
        let original = source.next().await.unwrap().unwrap();
        let outputs = transformer
            .transform(InputEnvelope {
                port: PortId::try_new("in").unwrap(),
                envelope: original.envelope.clone(),
            })
            .await
            .unwrap();
        assert_eq!(outputs[0].envelope.system().sequence(), sequence);
        assert_eq!(
            outputs[0].envelope.lineage().unwrap().envelope_id(),
            original.envelope.id()
        );
        assert_eq!(outputs[0].envelope.annotations().entries().count(), 1);
        sink.handle(InputEnvelope {
            port: PortId::try_new("in").unwrap(),
            envelope: outputs[0].envelope.clone(),
        })
        .await
        .unwrap();
        transformer.delivery_completed(&outputs).await.unwrap();
    }
    assert!(source.next().await.unwrap().is_none());
    assert_eq!(source.configuration().unwrap()["seen"], 2);
    assert_eq!(sink.configuration().unwrap()["seen"], 2);
    source.stop().await.unwrap();
    transformer.stop().await.unwrap();
    sink.stop().await.unwrap();
    let mut service = plugin.factories()[3]
        .create_component(ComponentId::try_new("service").unwrap(), json!({}))
        .unwrap();
    service.start().await.unwrap();
    service.run().await.unwrap();
    service.quiesce().await.unwrap();
    assert_eq!(service.configuration().unwrap()["seen"], 1);
    service.stop().await.unwrap();
}

#[tokio::test]
async fn native_failures_cancellation_and_schema_validation_are_not_success() {
    let plugin = plugin();
    let source = &plugin.factories()[0];
    for config in [
        json!({"count":"bad"}),
        json!({"fail_create":true}),
        json!({"wrong_descriptor":true}),
    ] {
        assert!(source
            .create_component(ComponentId::try_new("invalid").unwrap(), config)
            .is_err());
    }
    let mut failing = source
        .create_component(
            ComponentId::try_new("failed").unwrap(),
            json!({"fail_start":true}),
        )
        .unwrap();
    assert!(failing.start().await.is_err());
    assert!(failing.next().await.is_err());
    let mut blocked = source
        .create_component(
            ComponentId::try_new("blocked").unwrap(),
            json!({"blocked":true}),
        )
        .unwrap();
    blocked.start().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), blocked.next())
            .await
            .is_err()
    );
    assert_eq!(blocked.configuration().unwrap()["seen"], 0);
    blocked.stop().await.unwrap();
    let original = envelope(&ComponentId::try_new("input").unwrap(), 1);
    let bytes = blocked.codec.encode(&original).unwrap();
    assert_eq!(blocked.codec.decode(&bytes).unwrap().id(), original.id());
    let mut bad: Value = serde_json::from_slice(&bytes).unwrap();
    bad["operations"][0]["Add"]["after"]["bytes"] = json!([1, 2, 3]);
    assert!(
        blocked
            .codec
            .decode(&serde_json::to_vec(&bad).unwrap())
            .is_err(),
        "remote producer validator rejects malformed typed record bytes"
    );
    let mut bad: Value = serde_json::from_slice(&bytes).unwrap();
    bad["schema"]["definition"] = json!([255]);
    assert!(blocked
        .codec
        .decode(&serde_json::to_vec(&bad).unwrap())
        .is_err());
    let mut bad: Value = serde_json::from_slice(&bytes).unwrap();
    bad["format"] = json!(123);
    assert!(blocked
        .codec
        .decode(&serde_json::to_vec(&bad).unwrap())
        .is_err());
    let mut transformer = plugin.factories()[1]
        .create_component(
            ComponentId::try_new("t").unwrap(),
            json!({"fail_transform":true}),
        )
        .unwrap();
    transformer.start().await.unwrap();
    assert!(transformer
        .transform(InputEnvelope {
            port: PortId::try_new("in").unwrap(),
            envelope: original
        })
        .await
        .is_err());
    transformer.stop().await.unwrap();
}

struct NotifyingSink {
    descriptor: ComponentDescriptor,
    control: Option<ComponentControl>,
    seen: Arc<AtomicUsize>,
}
#[async_trait]
impl ComputationComponent for NotifyingSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.control = Some(control);
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for NotifyingSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        self.seen.fetch_add(1, Ordering::SeqCst);
        self.control
            .as_ref()
            .unwrap()
            .notify_upstream(ControlNotification::Available)?;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn host_graph_factories_and_control_progress_while_native_data_is_pending() {
    let plugin = plugin();
    let source = plugin.factories()[4].clone();
    let spec = source
        .specification(ComponentId::try_new("native").unwrap(), json!({"count":3}))
        .unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let sink = NotifyingSink {
        descriptor: ComponentDescriptor::try_new(
            ComponentId::try_new("sink").unwrap(),
            vec![port(PortDirection::Input)],
        )
        .unwrap(),
        control: None,
        seen: seen.clone(),
    };
    let mut registry = FactoryRegistry::default();
    plugin.register_factories(&mut registry).unwrap();
    assert!(plugin.register_factories(&mut registry).is_err());
    let mut graph = ComputationGraph::builder("native-control-test")
        .component(spec, source)
        .sink(Box::new(sink))
        .connect(
            EdgeDefinition {
                from: Endpoint::new(
                    ComponentId::try_new("native").unwrap(),
                    PortId::try_new("out").unwrap(),
                ),
                to: Endpoint::new(
                    ComponentId::try_new("sink").unwrap(),
                    PortId::try_new("in").unwrap(),
                ),
            },
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(
            Endpoint::new(
                ComponentId::try_new("native").unwrap(),
                PortId::try_new("out").unwrap(),
            ),
            StreamId::try_new("native/out").unwrap(),
        )
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), graph.start().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 3);
}

#[test]
fn strict_metadata_roles_versions_schema_and_required_fields() {
    let plugin = plugin();
    let mut metadata = plugin.metadata().clone();
    metadata.factories[0].role = ComponentRole::Query;
    assert!(metadata.validate().is_err());
    let mut metadata = plugin.metadata().clone();
    metadata.factories[0].capabilities.transactional = true;
    assert!(metadata.validate().is_err());
    let mut metadata = plugin.metadata().clone();
    metadata.factories[0].configuration_version = 0;
    assert!(metadata.validate().is_err());
    let mut metadata = plugin.metadata().clone();
    metadata.schemas.clear();
    assert!(metadata.validate().is_err());
    let mut json = serde_json::to_value(plugin.metadata()).unwrap();
    json.as_object_mut().unwrap().remove("factories");
    assert!(serde_json::from_value::<PluginMetadata>(json).is_err());
    let mut json = serde_json::to_value(plugin.metadata()).unwrap();
    json["factories"][0]["capabilities"]["query_snapshot_outbox"] = json!(true);
    assert!(serde_json::from_value::<PluginMetadata>(json).is_err());
    let mut spec = plugin.factories()[0]
        .specification(ComponentId::try_new("s").unwrap(), json!({}))
        .unwrap();
    spec.configuration_version += 1;
    assert!(ComponentFactory::validate(plugin.factories()[0].as_ref(), &spec).is_err());
    assert_eq!(drasi_plugin_sdk::ffi::FFI_SDK_VERSION, "0.15.0");
}

#[test]
fn transaction_factory_opt_in_and_linear_schema_contract_are_enforced() {
    let plugin = plugin();
    let factory = plugin.transactional_factories().pop().unwrap();
    let mut registry = TransactionalTransformerRegistry::default();
    registry.register(factory.clone()).unwrap();
    let mut definition = TransactionTransformerDefinition {
        graph_id: "native-transaction-contract".into(),
        id: ComponentId::try_new("container").unwrap(),
        output_stream: StreamId::try_new("container/out").unwrap(),
        steps: ["first", "second"]
            .into_iter()
            .map(|id| TransactionStepDefinition {
                id: ComponentId::try_new(id).unwrap(),
                implementation: factory.implementation(),
                configuration_version: 1,
                configuration: json!({}),
            })
            .collect(),
        outbox_capacity: NonZeroUsize::new(2).unwrap(),
    };
    let descriptor = definition.descriptor(&registry).unwrap();
    assert_eq!(descriptor.ports().len(), 2);
    assert_eq!(
        descriptor.ports()[0].schema(),
        GraphChangeCodec::schema().descriptor()
    );
    let participant = factory.create(&definition.steps[0]).unwrap();
    assert_eq!(participant.configuration().unwrap()["seen"], 0);
    assert!(participant.wakeup_source().is_none());
    assert!(!participant.has_pending_emissions());
    definition.steps[0].configuration_version = 2;
    assert!(definition.descriptor(&registry).is_err());
    assert!(
        TransactionalTransformerFactory::create(
            plugin.factories()[1].as_ref(),
            &definition.steps[1]
        )
        .is_err(),
        "an ordinary transformer cannot opt in by merely claiming a capability"
    );
}

#[test]
fn rejects_missing_or_incompatible_native_metadata_before_entry() {
    unsafe extern "C" fn no_metadata() -> *const abi::Metadata {
        std::ptr::null()
    }
    unsafe extern "C" fn must_not_enter(_: *mut abi::PluginHandle) -> abi::Status {
        // Returning an error lets this test fail, without unwinding through C.
        sdk::transport::status_result(Err(sdk::transport::Failure::failed(
            "entry must not execute",
        )))
    }
    assert!(
        unsafe { NativePlugin::from_entry_points(no_metadata, must_not_enter) }
            .unwrap_err_string()
            .contains("null")
    );
    #[repr(transparent)]
    struct SharedMetadata(abi::Metadata);
    unsafe impl Sync for SharedMetadata {}
    static BAD: SharedMetadata = SharedMetadata(abi::Metadata {
        header: abi::Header {
            major: 0,
            ..abi::Header::new::<abi::Metadata>()
        },
        target: abi::BorrowedBytes::empty(),
        manifest: abi::BorrowedBytes::empty(),
    });
    unsafe extern "C" fn bad_metadata() -> *const abi::Metadata {
        &BAD.0
    }
    let error = unsafe { NativePlugin::from_entry_points(bad_metadata, must_not_enter) }
        .unwrap_err_string();
    assert!(error.contains("incompatible native computation ABI header"));
}

trait ErrorString {
    fn unwrap_err_string(self) -> String;
}
impl<T> ErrorString for anyhow::Result<T> {
    fn unwrap_err_string(self) -> String {
        match self {
            Ok(_) => panic!("expected failure"),
            Err(error) => error.to_string(),
        }
    }
}
