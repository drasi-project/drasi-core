// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host services a factory declares are bound together and reach the plugin
//! through its single create entry; only undeclared or missing required
//! services are refused.

use super::*;
use anyhow::Context as _;
use drasi_core::computation::InMemoryComputationProvider;

const GRAPH: &str = "native-service-bindings";
const PRODUCER: u128 = 0x5e4;

fn scope() -> sdk::Scope {
    sdk::Scope {
        instance_id: GRAPH.into(),
        graph_id: GRAPH.into(),
        generation: 1,
    }
}

#[derive(Default)]
struct Received {
    processing: AtomicUsize,
    consumer_progress: AtomicUsize,
}

/// An indexed transformer that also reads its consumers' progress: a
/// combination the Host used to refuse.
struct Factory(Arc<Received>);

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/combined-services", "1")
                .unwrap(),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: sdk::ConfigSchema::default(),
            ports: vec![port(PortDirection::Input), port(PortDirection::Output)],
            completion: None,
            capabilities: sdk::Capabilities {
                indexed_processing: true,
                services: sdk::HostServices {
                    consumer_progress: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }
    fn configuration_details(&self) -> sdk::ConfigDetails {
        sdk::ConfigDetails::default()
    }
    fn describe(
        &self,
        _: &sdk::wire::DescribeRequest,
    ) -> anyhow::Result<sdk::ComponentDescription> {
        Ok(sdk::ComponentDescription::undeclared())
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        services: sdk::ServiceBindings,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let processing = services.processing.context("processing is bound")?;
        let progress = services
            .consumer_progress
            .context("consumer progress is bound")?;
        self.0.processing.fetch_add(1, Ordering::SeqCst);
        self.0.consumer_progress.fetch_add(1, Ordering::SeqCst);
        Ok(sdk::Component::Transformer(Box::new(Transformer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            reader: progress.reader(),
            _processing: processing,
        }))
        .into())
    }
}

struct Transformer {
    descriptor: ComponentDescriptor,
    reader: ConsumerProgressView,
    _processing: sdk::NativeProcessing,
}

#[async_trait]
impl sdk::NativeComponent for Transformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<Value> {
        Ok(json!({}))
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        Some(self.reader.clone())
    }
    drasi_computation_plugin_sdk::passive_lifecycle!();
}

#[async_trait]
impl sdk::NativeTransformer for Transformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}

fn plugin(received: Arc<Received>) -> anyhow::Result<Arc<NativePlugin>> {
    NativePlugin::from_definition(sdk::PluginDefinition::new(
        "fixture/combined-services",
        "1",
        vec![Arc::new(Factory(received))],
        vec![GraphChangeCodec::schema()],
    )?)
}

fn processing() -> Arc<super::super::NativeProcessingResource> {
    Arc::new(super::super::NativeProcessingResource::new(Arc::new(
        InMemoryComputationProvider,
    )))
}

fn consumers() -> anyhow::Result<Arc<ConsumerProgressSet>> {
    Ok(Arc::new(ConsumerProgressSet::new(
        GRAPH,
        component_id(PRODUCER),
        [Arc::new(QuerySourceProgress::new(
            GRAPH,
            component_id(0x5e5),
        )?)],
    )?))
}

#[test]
fn declared_services_are_bound_together_through_one_create() -> anyhow::Result<()> {
    let received = Arc::new(Received::default());
    let plugin = plugin(received.clone())?;
    let factory = &plugin.factories()[0];
    let dependencies = &factory.descriptor().dependencies;
    assert_eq!(dependencies["processing"].minimum, 1);
    assert_eq!(dependencies["consumer_progress"].minimum, 1);
    let component = factory.create_component_with_services(
        component_id(PRODUCER),
        json!({}),
        scope(),
        NativeServices {
            processing: Some(processing()),
            consumer_progress: Some(consumers()?),
            ..Default::default()
        },
    )?;
    assert_eq!(received.processing.load(Ordering::SeqCst), 1);
    assert_eq!(received.consumer_progress.load(Ordering::SeqCst), 1);
    drop(component);

    let mut specification = factory.specification(component_id(PRODUCER), json!({}))?;
    for name in ["processing", "consumer_progress"] {
        specification
            .dependencies
            .insert(name.into(), vec![ResourceId::try_new(name)?]);
    }
    factory.validate(&specification)?;
    Ok(())
}

#[test]
fn only_undeclared_and_missing_required_services_are_refused() -> anyhow::Result<()> {
    let received = Arc::new(Received::default());
    let plugin = plugin(received.clone())?;
    let factory = &plugin.factories()[0];
    let error = factory
        .create_component_with_services(
            component_id(PRODUCER),
            json!({}),
            scope(),
            NativeServices {
                processing: Some(processing()),
                ..Default::default()
            },
        )
        .err()
        .expect("consumer progress is required");
    assert!(
        error.to_string().contains("consumer progress binding"),
        "{error:#}"
    );
    let error = factory
        .create_component_with_services(
            component_id(PRODUCER),
            json!({}),
            scope(),
            NativeServices {
                processing: Some(processing()),
                consumer_progress: Some(consumers()?),
                source_progress: Some(Arc::new(QuerySourceProgress::new(
                    GRAPH,
                    component_id(0x5e6),
                )?)),
                ..Default::default()
            },
        )
        .err()
        .expect("source progress is not declared");
    assert!(
        error
            .to_string()
            .contains("does not declare source progress"),
        "{error:#}"
    );
    assert_eq!(received.processing.load(Ordering::SeqCst), 0);
    let mut specification = factory.specification(component_id(PRODUCER), json!({}))?;
    specification.dependencies.insert(
        "processing".into(),
        vec![ResourceId::try_new("processing")?],
    );
    assert!(
        factory.validate(&specification).is_err(),
        "the required consumer progress resource is missing"
    );
    specification.dependencies.insert(
        "consumer_progress".into(),
        vec![ResourceId::try_new("progress")?],
    );
    specification
        .dependencies
        .insert("admission".into(), vec![ResourceId::try_new("journal")?]);
    let error = factory.validate(&specification).err().unwrap();
    assert!(error.to_string().contains("'admission'"), "{error:#}");
    Ok(())
}
