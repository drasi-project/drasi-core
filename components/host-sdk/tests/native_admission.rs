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

/// A source that declares source admission when its flag is set.
struct Factory(bool);
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
            capabilities: sdk::Capabilities {
                services: sdk::HostServices {
                    source_admission: self.0,
                    ..Default::default()
                },
                ..sdk::Capabilities::default()
            },
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        services: sdk::ServiceBindings,
    ) -> Result<sdk::CreatedComponent> {
        let admission = services.admission;
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
impl sdk::NativeComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    drasi_computation_plugin_sdk::passive_lifecycle!();
}
#[async_trait]
impl sdk::NativeSource for Source {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        assert!(
            self.admission.is_none(),
            "native admission source must not be polled"
        );
        Ok(None)
    }
}

fn plugin(admission: bool) -> Result<Arc<NativePlugin>> {
    NativePlugin::from_definition(sdk::PluginDefinition::new(
        "probe/admission",
        "1",
        vec![Arc::new(Factory(admission))],
        vec![GraphChangeCodec::schema()],
    )?)
}

struct Sink {
    descriptor: ComponentDescriptor,
    seen: Arc<Mutex<Vec<u64>>>,
}
#[async_trait]
impl ComputationComponent for Sink {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
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
fn only_declared_admission_grants_its_resource() -> Result<()> {
    let old = plugin(false)?;
    let new = plugin(true)?;
    assert!(
        new.metadata().factories[0]
            .capabilities
            .services
            .source_admission
    );
    assert!(old.factories()[0].descriptor().dependencies.is_empty());
    let dependencies = &new.factories()[0].descriptor().dependencies;
    assert_eq!(dependencies["admission"].minimum, 0);
    assert_eq!(dependencies["admission"].maximum, Some(1));
    let mut spec = old.factories()[0].specification(
        ComponentId::try_new("55f1777b-fd8e-511e-ae80-87fec79a2466")?,
        serde_json::json!({}),
    )?;
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
        provider
            .create_indexes(
                "graph",
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000129")
                    .expect("fixture UUID"),
            )
            .await?,
        FactoryRegistry::standard().envelope_codec(NonZeroUsize::new(1024 * 1024).unwrap())?,
        "journal",
    )
    .await?;
    channel
        .enable_admission(AdmissionOptions {
            construction_scope: "graph".into(),
            graph_id: "graph".into(),
            component_id: ComponentId::try_new("55f1777b-fd8e-511e-ae80-87fec79a2466")?,
            failure_scope: FailureMode::ProcessRestart,
            max_producers: NonZeroUsize::new(2).unwrap(),
            receipts_per_producer: NonZeroUsize::new(2).unwrap(),
        })
        .await?;
    let plugin = plugin(true)?;
    let factory = plugin.factories()[0].clone();
    let source = ComponentId::try_new("55f1777b-fd8e-511e-ae80-87fec79a2466")?;
    let sink = ComponentId::try_new("9c084f4c-64cc-5879-a3c3-e1de29bea7b9")?;
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
        .component(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-component-1")
                .expect("fixture component name"),
            spec,
            factory,
        )
        .sink(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-sink-1")
                .expect("fixture component name"),
            Box::new(Sink {
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
            }),
        )
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
        // Producer metadata is served while the source is not yet active, so
        // a client can resolve an uncertain outcome during a Pause, but no
        // input is admitted without an activation.
        let parked = service
            .register_producer(&ComponentId::try_new(
                "e1b1c4a6-0f3b-5d8e-9c1a-3b7d2f4e6a80",
            )?)
            .await?;
        let early = ChangeEnvelope::new(
            EnvelopeId::try_new("input", bytes::Bytes::from_static(b"early"))?,
            ChangeSet::try_new(
                ChangeSetId::try_new("input", bytes::Bytes::from_static(b"early"))?,
                GraphChangeCodec::schema().descriptor().clone(),
                vec![],
            )?,
            SystemMetadata::new(definition.stream.clone(), 1),
        );
        assert_eq!(
            service.admit(&parked, 1, &early).await.unwrap_err().kind,
            sdk::AdmissionErrorKind::Closed
        );
        assert_eq!(service.admission_receipt(&parked, 1).await?, None);
        service.retire_producer(&parked).await?;
        let started = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await?;
        assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
        let session = loop {
            match service
                .register_producer(&ComponentId::try_new(
                    "5ddadfa1-268c-5a36-80fa-e43b8baac458",
                )?)
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
        while control.observed().components
            [&ComponentId::try_new("55f1777b-fd8e-511e-ae80-87fec79a2466")?]
            .failure
            .is_none()
        {
            tokio::task::yield_now().await;
        }
        assert!(format!(
            "{:?}",
            control.observed().components
                [&ComponentId::try_new("55f1777b-fd8e-511e-ae80-87fec79a2466")?]
                .failure
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
            .register_producer(&ComponentId::try_new(
                "d8497dcf-ded6-590b-851c-9b9b96ce6cf9"
            )?)
            .await
            .unwrap_err()
            .kind,
        sdk::AdmissionErrorKind::Closed
    );
    channel.shutdown().await?;
    Ok(())
}
