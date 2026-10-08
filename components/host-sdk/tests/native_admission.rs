// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_plugin_sdk::{self as sdk, abi};
use drasi_core::{computation::ComputationIndexProvider, interface::FailureMode};
use drasi_host_sdk::computation::NativePlugin;
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::computation::v1::*;

fn captured() -> &'static Mutex<Option<sdk::NativeAdmission>> {
    static HANDLE: OnceLock<Mutex<Option<sdk::NativeAdmission>>> = OnceLock::new();
    HANDLE.get_or_init(|| Mutex::new(None))
}

struct Factory;
impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("probe/source", "1")
                .expect("test implementation"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: sdk::ConfigSchema::default(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("out").expect("test port"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: None,
            capabilities: sdk::Capabilities::default(),
        }
    }
    fn supports_source_admission(&self) -> bool {
        true
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        control: sdk::ControlSender,
    ) -> Result<sdk::CreatedComponent> {
        self.create_with_admission(request, control, None)
    }
    fn create_with_admission(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        admission: Option<sdk::NativeAdmission>,
    ) -> Result<sdk::CreatedComponent> {
        *captured().lock().expect("capture lock") = admission.clone();
        Ok(sdk::Component::Source(Box::new(Source {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            admission,
        }))
        .into())
    }
}

struct Source {
    descriptor: ComponentDescriptor,
    admission: Option<sdk::NativeAdmission>,
}
#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        assert!(
            self.admission.is_none(),
            "native admission source must not be polled"
        );
        Ok(None)
    }
}

mod fixture {
    use super::*;
    sdk::export_computation_plugin!(sdk::PluginDefinition::new(
        "probe/admission",
        "1",
        vec![Arc::new(Factory)],
        vec![GraphChangeCodec::schema()],
    ));
}

fn plugin(services: bool) -> Result<Arc<NativePlugin>> {
    unsafe {
        NativePlugin::from_entry_points_with_services(
            fixture::drasi_computation_plugin_metadata,
            fixture::drasi_computation_plugin_entry,
            services.then_some(fixture::drasi_computation_plugin_services_v1),
        )
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    seen: Arc<Mutex<Vec<u64>>>,
}
#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.seen
            .lock()
            .expect("capture lock")
            .push(input.envelope.system().sequence());
        Ok(())
    }
}

#[test]
fn service_negotiation_does_not_change_old_metadata_or_grant_undeclared_resources() -> Result<()> {
    let old = plugin(false)?;
    let new = plugin(true)?;
    assert_eq!(old.metadata(), new.metadata());
    assert!(old.factories()[0].descriptor().dependencies.is_empty());
    let dependencies = &new.factories()[0].descriptor().dependencies;
    assert_eq!(dependencies["admission"].minimum, 0);
    assert_eq!(dependencies["admission"].maximum, Some(1));
    let mut spec =
        old.factories()[0].specification(ComponentId::try_new("source")?, serde_json::json!({}))?;
    spec.dependencies.insert(
        Arc::from("admission"),
        vec![ResourceId::try_new("journal")?],
    );
    assert!(old.factories()[0].validate(&spec).is_err());
    new.factories()[0].validate(&spec)?;
    spec.dependencies
        .insert(Arc::from("arbitrary-store"), vec![]);
    assert!(new.factories()[0].validate(&spec).is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_admission_uses_graph_commit_and_revokes_retained_callbacks() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
        directory.path(),
        false,
        false,
    )));
    let definition = QosChannelDefinition {
        stream: StreamId::try_new("source/out")?,
        capacity: NonZeroUsize::new(2).unwrap(),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([("sink".into(), SubscriptionStart::Earliest)]),
    };
    let channel = QosChannel::persistent(
        definition.clone(),
        provider.create_indexes("graph", "channel").await?,
        FactoryRegistry::standard().envelope_codec(NonZeroUsize::new(1024 * 1024).unwrap())?,
        "journal",
    )
    .await?;
    channel
        .enable_admission(AdmissionOptions {
            construction_scope: "graph".into(),
            graph_id: "graph".into(),
            component_id: ComponentId::try_new("source")?,
            failure_scope: FailureMode::ProcessRestart,
            max_producers: NonZeroUsize::new(2).unwrap(),
            receipts_per_producer: NonZeroUsize::new(2).unwrap(),
        })
        .await?;
    let plugin = plugin(true)?;
    let factory = plugin.factories()[0].clone();
    let source = ComponentId::try_new("source")?;
    let sink = ComponentId::try_new("sink")?;
    let resource = ResourceId::try_new("journal")?;
    let mut spec = factory.specification(source.clone(), serde_json::json!({}))?;
    spec.dependencies
        .insert(Arc::from("admission"), vec![resource.clone()]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut graph = ComputationGraph::builder("graph")
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "journal".into(),
        })?
        .provide_resource(resource.clone(), channel.resource())?
        .component(spec, factory)
        .sink(Box::new(Sink {
            descriptor: ComponentDescriptor::try_new(
                sink.clone(),
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            seen: seen.clone(),
        }))
        .bind_stream(
            Endpoint::new(source.clone(), PortId::try_new("out")?),
            definition.stream.clone(),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(source, PortId::try_new("out")?),
                Endpoint::new(sink, PortId::try_new("in")?),
            ),
            Box::new(definition.pipe(resource, "sink")),
        )
        .build()?;
    let run = graph.run()?;
    let control = run.control();
    let client = async {
        let deployment = control.deployment_report().await?;
        assert_eq!(
            deployment.summary,
            OperationSummary::Completed,
            "{deployment:?}"
        );
        let service = captured()
            .lock()
            .unwrap()
            .take()
            .expect("injected native service");
        assert_eq!(
            service
                .register_producer(&ComponentId::try_new("client")?)
                .await
                .unwrap_err()
                .kind,
            sdk::AdmissionErrorKind::Closed
        );
        let started = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await?;
        assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
        let session = loop {
            match service
                .register_producer(&ComponentId::try_new("client")?)
                .await
            {
                Err(error) if error.kind == sdk::AdmissionErrorKind::Closed => {
                    tokio::task::yield_now().await
                }
                result => break result?,
            }
        };
        let input = ChangeEnvelope::new(
            EnvelopeId::try_new("input", bytes::Bytes::from_static(b"one"))?,
            ChangeSet::try_new(
                ChangeSetId::try_new("input", bytes::Bytes::from_static(b"one"))?,
                GraphChangeCodec::schema().descriptor().clone(),
                vec![],
            )?,
            SystemMetadata::new(definition.stream.clone(), 1),
        );
        let receipt = service.admit(&session, 1, &input).await?;
        assert_eq!(receipt.position, 1);
        loop {
            match service.admit(&session, 1, &input).await {
                Err(error) if error.kind == sdk::AdmissionErrorKind::Busy => {
                    tokio::task::yield_now().await
                }
                result => {
                    assert_eq!(result?, receipt);
                    break;
                }
            }
        }
        assert_eq!(
            service.producer_status(&session).await?.next_sequence,
            Some(2)
        );
        assert_eq!(service.admission_receipt(&session, 1).await?, Some(receipt));
        while channel.progress().await?.processed["sink"] != 1 {
            tokio::task::yield_now().await;
        }
        service.retire_producer(&session).await?;
        assert_eq!(
            service.producer_status(&session).await.unwrap_err().kind,
            sdk::AdmissionErrorKind::SessionExpired
        );
        service.report_failure("deliberate listener failure")?;
        while control.observed().components[&ComponentId::try_new("source")?]
            .failure
            .is_none()
        {
            tokio::task::yield_now().await;
        }
        assert!(format!(
            "{:?}",
            control.observed().components[&ComponentId::try_new("source")?].failure
        )
        .contains("deliberate listener failure"));
        Ok::<_, anyhow::Error>(service)
    };
    let (result, client) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, async {
            let result = client.await;
            control.cancel();
            result
        })
    })
    .await?;
    let service = client?;
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(*seen.lock().unwrap(), [1]);
    assert_eq!(channel.progress().await?.accepted, 1);
    graph.shutdown().await?;
    drop(graph);
    assert_eq!(
        service
            .register_producer(&ComponentId::try_new("late")?)
            .await
            .unwrap_err()
            .kind,
        sdk::AdmissionErrorKind::Closed
    );
    channel.shutdown().await?;
    Ok(())
}

#[test]
fn malformed_service_extension_is_rejected_before_plugin_entry() {
    unsafe extern "C" fn invalid() -> *const abi::services::PluginServicesV1 {
        static TABLE: abi::services::PluginServicesV1 = abi::services::PluginServicesV1 {
            header: abi::Header::new::<abi::services::PluginServicesV1>(),
            version: 99,
            reserved: 0,
            factory: None,
            create: None,
        };
        &TABLE
    }
    unsafe extern "C" fn never_enter(_: *mut abi::PluginHandle) -> abi::Status {
        panic!("invalid extension must reject before entry")
    }
    assert!(unsafe {
        NativePlugin::from_entry_points_with_services(
            fixture::drasi_computation_plugin_metadata,
            never_enter,
            Some(invalid),
        )
    }
    .is_err());
}
